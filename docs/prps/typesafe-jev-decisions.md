name: "PRP: TypeSafe AI Jev for supervisory decisions"
description: |
  Route batty's supervisory yes/no and choose-one decisions through TypeSafe AI's Jev decision
  model on the Vercel AI Gateway, with today's heuristics as the fallback. Covers completion
  checks (veto only), a review rubric (advisory), manager/architect inbox triage, screen state and
  stall reasons. Off by default.

## Purpose

AI implementation guide with complete context, validation commands and patterns to mirror, so the feature can be built in one pass. Follow `CLAUDE.md`: minimal code, existing patterns, `#[cfg(test)]` coverage for every module, and `cargo test` green before each commit. No `allow(dead_code)`.

Task breakdown: [docs/tasks/typesafe-jev-decisions.md](../tasks/typesafe-jev-decisions.md)

---

## Discovery Summary

### Initial Task Analysis

The request was to "enable support for typesafe AI JEV". [Jev](https://vercel.com/ai-gateway/models/jev) is TypeSafe AI's "System One decision model" on the Vercel AI Gateway (`typesafe-ai/jev`, pinned `typesafe-ai/jev-1.14.0`). It answers typed questions (`predicate`, `choice`, `score`) about a block of state text, with all questions evaluated in parallel. It costs $0.04 per 1M input tokens and has a 32K context.

The brainstorm is `docs/brainstorming/2026-11-07-typesafe-jev-decisions.md`.

### User Clarifications Received

- **Q:** Which decisions? **A:** All five candidates: completion checks, review rubric, inbox triage, screen state, and stall detection.
- **Q:** What authority does Jev get? **A:** It is primary, with the heuristic as the fallback.
- **Q:** What data may be sent? **A:** Content may be sent, but keys and secrets are redacted first.
- **Q:** When should Jev be called? **A:** Only when the redacted input changed since the last answer. Only for the manager and architect, meaning the decisions those roles own. Engineer screen, stall and routing paths stay on the heuristics.
- **Preflight defaults (accepted):**
  1. Screen state applies only to manager/architect members on the PTY runtime. It is computed daemon-side from a shim screen snapshot, using choice `idle|working|context_exhausted|unknown` mapped 1:1 to `ScreenVerdict`. SDK-mode members are unchanged.
  2. Completion is **veto only**. Jev can add a reject (`NarrationOnly` or `InsufficientEvidence`) to a completion the heuristics accept. It never clears `NoCommits`, `TestsFailed` or the merge gates.
  3. Review rubric is new. Each acceptance criterion parsed from the task gets a score, plus a `criteria_met` predicate. It is advisory only: an event and a manager message line. It is not part of the bench bar.
  4. Triage: `noise` and `status_reply` are suppressed, like `orchestrator_only_reason` returning `Some`. `actionable` and `escalation` are delivered, with escalation marked as priority.
  5. A schema-invalid answer, a refusal, or confidence below 0.6 falls back to the heuristic and emits a `decision_fallback` event.
  6. Budget comes from the cost in the response. The daily total is persisted in `telemetry.db`, keyed by UTC date. The cap defaults to $5.
  7. The feature is off by default (`provider: heuristic`). A per-path enable list controls each path.
  8. `zeroDataRetention: true` is sent on every request.
  9. Stall: Jev only adds a reason when the heuristic reports `None`. It never overrides `Expected` or `Incidental`.
- **Technical decisions (made during research, recorded as assumptions):**
  - Screen state from Jev updates the **daemon's** view only: `MemberState` and `AgentHandle` state. The shim's internal state machine is unchanged.
  - A Jev "not stalled" answer never suppresses a stall.
  - The rubric is sent as a **follow-up** manager message when its job finishes.
  - On the live delivery path (`deliver_message_with_id`), a triage cache miss uses the heuristic and starts a job for the next time. The inbox path (`deliver_inbox_messages`) defers the message until the answer arrives or the timeout passes, because it re-reads each tick anyway.

### Missing Requirements Identified

- The daemon drops `Event::ScreenCapture` today (`poll_shim.rs:306-309`). A handler is needed.
- `on_completion` drops the agent's response before calling the gates (`shim_events.rs:303,313`). The response must be stashed for the veto.
- There is no acceptance-criteria parser. The `## Acceptance Criteria` heading comes from `spec_gen.rs:124,241`.
- There is no secret-pattern redaction (only `http::redact_secret` for a known literal). The redaction module is new.
- No test uses a fake HTTP server. A tiny `TcpListener` test server is needed.
- The daemon is single-threaded (threads plus mpsc, no tokio), so Jev calls must run as background jobs.

## Goal

When `workflow_policy.decisions.provider: jev` is set and an API key is in the environment:

- Completion checks can veto empty or narration-only completions.
- Tasks entering review get a rubric message for the manager.
- Manager and architect inbox messages are triaged by Jev.
- Manager and architect PTY screen state is classified by Jev.
- Supervisory stalls with no signal get a Jev reason.

Every path falls back to today's heuristic within the same poll when Jev is unavailable, slow, over budget, uncertain, or turned off. No network call ever blocks the poll loop, and no secret is ever sent or logged.

## Why

- Heuristic decisions (regexes, keywords, commit counts) are brittle on the paths that gate the whole team. Jev gives typed, cheap decisions at those points.
- The project priority is stability, then throughput. The fallback and the off-by-default setting keep stability, while better triage and completion checks improve throughput.

## What

**Config:**

```yaml
workflow_policy:
  decisions:
    provider: heuristic           # heuristic (default) | jev
    model: typesafe-ai/jev-1.14.0
    endpoint: https://ai-gateway.vercel.sh/v1/decisions
    api_key_env: AI_GATEWAY_API_KEY
    daily_spend_cap_usd: 5.0
    min_confidence: 0.6
    zero_data_retention: true
    timeouts_ms: { screen_state: 2000, triage: 2000, stall: 2000, completion: 10000, rubric: 10000 }
    paths: [completion, rubric, triage, screen_state, stall]
```

**Events:**

- `decision_made` (path, member, provider, latency_ms, cached, cost_usd)
- `decision_fallback` (path, member, reason: `disabled|no_key|budget|timeout|http_<code>|invalid_answer|refusal|low_confidence`)
- `decision_budget_exceeded` (once per UTC day)
- `completion_vetoed` (task, reason)
- `review_rubric` (task, criteria_met, scores)

### Success Criteria

- [ ] With `provider: heuristic` (the default), behaviour is identical to today and no HTTP calls are made.
- [ ] With `provider: jev` and a key:
  - each in-scope path makes at most one call per change of redacted input;
  - calls run off the poll thread;
  - answers apply on a later tick;
  - timeouts are honoured.
- [ ] A missing key, an exhausted budget, a timeout, an HTTP error, an invalid answer, a refusal or low confidence each produce the heuristic outcome plus one `decision_fallback` event. A missing key logs once at startup, not every poll.
- [ ] The completion veto rejects only completions that passed every hard gate, and only as `NarrationOnly` or `InsufficientEvidence`. A Jev error never blocks a completion.
- [ ] The rubric message is sent to the manager for tasks entering review. It is never part of the merge or bench decision.
- [ ] Triage applies only to manager and architect recipients. Engineers keep `is_engineer_ambient_chatter` and the rest.
- [ ] Screen state applies only to manager/architect members on the PTY runtime.
- [ ] Stall: Jev adds a reason only when the heuristic gives `SupervisoryProgress::None`.
- [ ] Redaction: every pattern below is replaced with `[REDACTED:<kind>]` before hashing, sending or logging. Unit tests cover each pattern plus false-positive cases.
- [ ] Spend is recorded from `provider_metadata.gateway.cost` and persisted per UTC day. Over the cap, every path is heuristic until the next day.
- [ ] `make lint` and `cargo test` pass. The docs are updated.

## All Needed Context

### Research Phase Summary

- **Codebase patterns found:**
  - `http::agent()` with `ureq` 2.12.1, a per-request `.timeout()`, and status/transport error redaction (Telegram/Discord clients).
  - The `TestJob` off-thread pattern and the `BackgroundTests` owner.
  - Nested `WorkflowPolicy` configs.
  - `telemetry_db` `CREATE TABLE IF NOT EXISTS` plus upsert.
  - `TeamEvent` constructors.
  - `TestDaemonBuilder` and the socketpair `AgentHandle` mocks.
- **External research needed:** yes. It was done for the Gateway Decisions HTTP API, with results below.
- **Knowledge gaps:**
  - not verified: the max questions per request, the default Gateway timeout, 5xx semantics, and the alias update policy;
  - therefore: pin the model, set client timeouts, treat 5xx as fallback with no retry, and keep at most 16 questions per request.

### Documentation & References

```yaml
- url: https://vercel.com/docs/ai-gateway/sdks-and-apis/openai-decisions
  why: The HTTP API to use, POST /v1/decisions, with its request and response shapes.
- url: https://vercel.com/docs/ai-gateway/modalities/decision
  why: Question types (predicate/choice/score), refusals, and the fact that decisions are NOT available via chat completions.
- url: https://vercel.com/ai-gateway/models/jev
  why: Model ids, pricing, 32K context, providers (typesafe-ai, digitalocean).
- url: https://vercel.com/docs/ai-gateway/models-and-providers/provider-options
  why: providerOptions.gateway.zeroDataRetention / order / only.
- url: https://vercel.com/docs/ai-gateway/faq
  why: Error codes (401/402 quota_for_entity_exceeded/403/429 with retry-after).
- url: https://vercel.com/changelog/typesafe-ais-jev-1-14-is-now-available-on-ai-gateway
  why: jev-1.14.0 pin; longContext is experimental (do not enable).
- file: src/team/http.rs
  why: Shared ureq agent (OnceLock; 5s connect / 15s read / 10s write). Use agent().post(url).timeout(d)
    for the 2s/10s budgets. redact_secret is the literal-redaction precedent.
- file: src/team/telegram.rs
  why: Request pattern at 180-222 (send_string + into_json, Status vs Transport errors redacted);
    a pure response parser tested separately (parse_send_message_response, test at 1052).
- file: src/team/test_job.rs
  why: TestJob<T> (94-245): spawn_with_timeout(label, Option<Duration>, FnOnce(&ChildPgid)->Result<T>),
    try_take() -> Some/None/TestTimedOut. Reuse with the pgid ignored.
- file: src/team/daemon/background_tests.rs
  why: BackgroundTests (19-73) holds keyed job maps; poll_background_tests (122-128) runs from tick().
- file: src/team/daemon/poll.rs
  why: tick() order: poll_shim_handles 268 -> check_working_state_timeouts 276 -> deliver_inbox_messages
    290 -> poll_background_tests 332. Add a recoverable "poll_decisions" step before 276.
- file: src/team/daemon.rs
  why: TeamDaemon fields (294-341) and the single constructor at 560. Add a `decisions` sub-state.
- file: src/shim/classifier.rs
  why: ScreenVerdict (10-19), classify (102), classify_with_confidence (112), MIN_CLASSIFIER_CONFIDENCE.
- file: src/shim/protocol.rs
  why: Command::CaptureScreen{last_n_lines} (23-25), Event::ScreenCapture{content,cursor_row,cursor_col}
    (132-136), Event::StateChanged (71-75).
- file: src/team/daemon/health/poll_shim.rs
  why: Event dispatch handle_shim_event (246); ScreenCapture is currently dropped at 306-309 (add a handler
    there). insert_mock_handle (1393-1409) for tests. check_working_state_timeouts (1059-1110).
- file: src/team/daemon/agent_handle.rs
  why: AgentHandle (24-60) has no sdk_mode field; send_ping (221) is the pattern for sending CaptureScreen.
- file: src/team/daemon/launcher.rs
  why: SDK mode is decided as agent_supports_sdk_mode(agent) && team_config.use_sdk_mode (189, 392, fn 817);
    use the same expression to decide PTY-runtime members.
- file: src/team/delivery/routing.rs
  why: orchestrator_only_reason (761-784; order: member_receives_pty_delivery, nudge, status query, standup,
    engineer chatter); OrchestratorOnlyReason enum (118-135); live path deliver_message_with_id (1104);
    inbox loop deliver_inbox_messages (1388; Maildir re-read per tick 1243-1260); manager notice
    classification (217, 559); tiered inbox QueueTier::Priority (inbox_tiered.rs:7,56; routing.rs:1172);
    uses_management_batching (699-707) for the role check; record_orchestrator_only_message (833-862).
- file: src/team/daemon/health/poll_shim/shim_events.rs
  why: on_completion (246-315) has `response`; it calls merge::handle_engineer_completion at 303 and drops
    the response (313). Stash a redacted response before 303.
- file: src/team/merge/completion/mod.rs
  why: handle_engineer_completion (73-150): DiffEvidence, CompletionContext (49-62), verification job start
    (143-146) or a synchronous finish when tests are skipped (149-150). finish_engineer_completion (154-218):
    veto insertion between 172 and 183, only when has_required_evidence && passed.
- file: src/team/merge/completion/gates.rs
  why: DiffEvidence (35-76), Rejection (126-136), Rejection::classify (138-150; yields TestsFailed when
    evidence is ok, so the veto needs an explicit rejection argument), has_required_evidence (227-245),
    reject_completion (250-366; NarrationOnly escalates at 2, at 287-291), block_escalated_task (372).
- file: src/team/merge/completion/verify_job.rs
  why: start_verification_job (167-197), poll_verification_jobs (233-257) -> apply_verification_result:
    where the veto result is joined.
- file: src/team/merge/completion/decision.rs
  why: route_to_manual_review (170-200) and route_override_to_review (134-166) tell the manager about
    review tasks; start the rubric job there. move_task_to_review (335).
- file: src/team/spec_gen.rs
  why: Emits "## Acceptance Criteria" (124, 241). The parser reads that section's bullet lines.
- file: src/team/daemon/health/stall.rs
  why: SupervisoryProgress (44-52) with its custom PartialEq (54-70); supervisory_progress_signal (498-542)
    returns None last; is_stall_signal (133); record_supervisory_stall_reason (572-622) emits
    stall_detected_with_reason; reasons are &'static str, so put the Jev reason in event.details.
- file: src/team/telemetry_db.rs
  why: init_schema execute_batch (254-357) for the new table; upsert precedent ON CONFLICT (536, 620);
    record_review_queue_snapshot / query_latest_review_queue_metric (1281-1325) style; open_in_memory (248).
    daemon.telemetry_db (daemon.rs:319) is poll-thread only: record spend when the result is taken.
- file: src/team/events.rs
  why: TeamEvent (67-140), the ..Self::base("name") constructor pattern (148; review_escalated 398;
    stall_detected_with_reason 786), serialization tests (1970).
- file: src/team/config/types.rs
  why: WorkflowPolicy (255), nested-policy fields (334-337), Default (445-446), per-field serde default
    pattern (492-550; BenchRecoveryConfig).
- file: src/team/config/tests_advanced.rs
  why: parse_bench_recovery_defaults / partial override / TestDaemonBuilder default (534-568) to mirror.
- file: src/team/test_support.rs
  why: TestDaemonBuilder (532-690); set decisions.provider = Heuristic by default as bench_recovery does.
- file: src/team/merge/completion/tests/gates.rs
  why: Gate test fixtures (250-330): init_git_repo, write_task_file, setup_engineer_worktree,
    make_test_daemon, handle_engineer_completion_and_wait.
- file: src/team/daemon/tests.rs
  why: Stall test pattern (2960-2996).
- docfile: docs/brainstorming/2026-11-07-typesafe-jev-decisions.md
```

### Gateway Decisions API (verified from official docs)

```bash
curl -X POST https://ai-gateway.vercel.sh/v1/decisions \
  -H "Authorization: Bearer $AI_GATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "typesafe-ai/jev-1.14.0",
    "input": "<redacted state text>",
    "questions": [
      {"type":"predicate","name":"done","instructions":"Did the agent complete the task?"},
      {"type":"choice","name":"kind","instructions":"Classify the message",
       "choices":[{"value":"actionable","description":"needs action"},{"value":"noise"}]},
      {"type":"score","name":"criterion_1","instructions":"How well is criterion 1 met?",
       "levels":[{"label":"not met"},{"label":"partly"},{"label":"met"}]}
    ],
    "providerOptions": {"gateway": {"zeroDataRetention": true}}
  }'
```

```json
{
  "model": "typesafe-ai/jev",
  "answers": [
    {"type":"predicate","name":"done","probability":0.98},
    {"type":"choice","name":"kind","choice":"actionable","confidence":0.81,
     "probabilities":[{"value":"actionable","probability":0.81},{"value":"noise","probability":0.19}]},
    {"type":"score","name":"criterion_1","score":1.3,"confidence":0.62,"probabilities":[...]}
  ],
  "usage": {"input_tokens":282,"output_tokens":20,"total_tokens":302},
  "provider_metadata": {"gateway": {"cost":"0.000011844", "generationId":"gen_..."}}
}
```

- `choice` takes 1–255 choices, `score` takes 2–10 levels, and question names must be unique. Answers come back in question order.
- `confidence` and `probabilities` are optional (`Option`).
- A refused question returns `{"type":"refusal","name":...}`, and the input is still billed.
- Errors use the OpenAI shape `{"error":{"message","type","param","code"}}`.
  - 401 means a bad key.
  - 402 means no credits, or `quota_for_entity_exceeded` (do not retry).
  - 403 means verification or allowlist.
  - 429 is `rate_limit_exceeded` with `retry-after`.
- `cost` is a **decimal string in USD**; parse it as f64 from the string and sum per day.
- Decisions are **not** available on chat completions. Do not use `/v1/chat/completions` or the deprecated `/v1/evaluate`.

### Current Codebase tree (relevant)

```bash
src/
  shim/classifier.rs, shim/protocol.rs, shim/runtime.rs
  team/
    http.rs telegram.rs discord.rs test_job.rs telemetry_db.rs events.rs spec_gen.rs
    config/types.rs config/tests_advanced.rs test_support.rs
    delivery/routing.rs inbox_tiered.rs
    merge/completion/{mod,gates,verify_job,decision,messaging}.rs
    daemon.rs daemon/{poll,background_tests,agent_handle,launcher}.rs
    daemon/health/{poll_shim,stall}.rs daemon/health/poll_shim/shim_events.rs
```

### Desired Codebase tree

```bash
src/team/decide/
  mod.rs          # Decider trait-free core: DecisionPath, Question, Answer, Verdict types; DecisionsState
  client.rs       # Gateway HTTP client: request builder + response parser (pure fns) + send()
  redact.rs       # secret redaction (regex set, OnceLock), redact(text) -> String
  budget.rs       # daily spend: read/add via telemetry_db; over_cap()
  gate.rs         # change gate: per (path,key) last hash + last answer; bounded LRU for triage
  paths.rs        # per-path question builders + answer->outcome mappers (pure, unit-tested)
  criteria.rs     # acceptance-criteria parser for the rubric
src/team/daemon/decisions.rs   # impl TeamDaemon: start jobs, poll_decisions step, apply results
```

### Known Gotchas of our codebase & Library Quirks

```rust
// CRITICAL: the daemon is single-threaded. Never call ureq on the poll thread. Every Jev call
//   is a TestJob; results are applied by a recoverable "poll_decisions" tick step that runs before
//   check_working_state_timeouts (poll.rs:276) and deliver_inbox_messages (poll.rs:290).
// CRITICAL: orchestrator_only_reason, supervisory_progress_signal, on_state_changed and
//   finish_engineer_completion are synchronous and take &self/&mut self. They must READ a cached
//   verdict from daemon.decisions, never call the network.
// CRITICAL: ureq 2 `Request::timeout(d)` is an overall deadline that overrides the agent's read/write
//   timeouts. Use it for the 2s/10s budgets; also wrap the job in TestJob with the same limit.
// CRITICAL: redact BEFORE hashing, sending, logging and storing in events. Raw content is logged by
//   append_shim_event_log (mod.rs:184), record_orchestrator_action previews (routing.rs:842-860),
//   `debug!(event = ?event)` (poll_shim.rs:307, which would dump ScreenCapture content), and
//   TeamEvent.details/reason (persisted to events.jsonl and SQLite payload). Never log the API key
//   or put it in a URL; redact ureq Transport errors (they include the URL).
// CRITICAL: Rejection::classify returns TestsFailed when evidence is OK, so the veto must pass an
//   explicit Rejection (NarrationOnly or InsufficientEvidence) to reject_completion. NarrationOnly
//   escalates after 2 rejections (gates.rs:287-291), which is the intended outcome for loops.
// CRITICAL: the completion veto must never block or delay a completion when Jev fails: on fallback,
//   proceed exactly as today. Run the veto job in parallel with the verification job and join it in
//   apply_verification_result. On the test-skipped path, wait at most completion timeout (10s)
//   across ticks, then proceed.
// CRITICAL: telemetry_db Connection is not Sync and lives on the poll thread. Record spend when the
//   job result is taken, never from the worker thread.
// CRITICAL: a missing API key => one decision_fallback(no_key) at startup, then silent heuristic.
// CRITICAL: the change-gate cache must be bounded: one entry per (path, member) plus a triage LRU (256).
// CRITICAL: TestDaemonBuilder must default decisions.provider = Heuristic so existing tests are untouched.
// GOTCHA: AgentHandle has no sdk_mode; compute PTY-runtime as
//   !(agent_supports_sdk_mode(agent) && team_config.use_sdk_mode).
// GOTCHA: stall reasons are &'static str; put the Jev reason text in TeamEvent.details.
// GOTCHA: the 32K context: cap input at ~24K tokens (~96K chars after redaction); send a diff summary
//   (stat + file list + first hunks), never the full diff; screen = last 100 lines.
// GOTCHA: no fake HTTP server exists; write a tiny std::net::TcpListener thread server in
//   decide/client.rs tests that returns canned JSON and status codes.
```

## Implementation Blueprint

### Data models and structure

```rust
// src/team/config/types.rs (mirror BenchRecoveryConfig)
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionProvider { #[default] Heuristic, Jev }

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionPath { Completion, Rubric, Triage, ScreenState, Stall }

pub struct DecisionsConfig {
    pub provider: DecisionProvider,         // heuristic
    pub model: String,                      // "typesafe-ai/jev-1.14.0"
    pub endpoint: String,                   // "https://ai-gateway.vercel.sh/v1/decisions"
    pub api_key_env: String,                // "AI_GATEWAY_API_KEY"
    pub daily_spend_cap_usd: f64,           // 5.0
    pub min_confidence: f64,                // 0.6
    pub zero_data_retention: bool,          // true
    pub timeouts_ms: DecisionTimeouts,      // 2000/2000/2000/10000/10000
    pub paths: Vec<DecisionPath>,           // all five
}

// src/team/decide/mod.rs
pub enum Question {
    Predicate { name: String, instructions: String },
    Choice { name: String, instructions: String, choices: Vec<(String, Option<String>)> },
    Score { name: String, instructions: String, levels: Vec<String> },
}
#[derive(Deserialize)] #[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Predicate { name: Option<String>, probability: f64 },
    Choice { name: Option<String>, choice: serde_json::Value, confidence: Option<f64> },
    Score { name: Option<String>, score: f64, confidence: Option<f64> },
    Refusal { name: Option<String> },
    #[serde(other)] Unknown,
}
pub struct DecisionResponse { pub answers: Vec<Answer>, pub cost_usd: f64, pub input_tokens: u64 }

pub enum Fallback { Disabled, NoKey, Budget, Timeout, Http(u16), InvalidAnswer, Refusal, LowConfidence }

// per-path outcomes (mapped from answers in decide/paths.rs)
pub enum ScreenOutcome { Idle, Working, ContextExhausted, Unknown }          // -> ScreenVerdict 1:1
pub enum TriageOutcome { Actionable, StatusReply, Noise, Escalation }
pub struct CompletionOutcome { pub done: bool, pub narration_only: bool }   // veto if !done or narration_only
pub struct RubricOutcome { pub criteria_met: bool, pub scores: Vec<(String, f64)> }
pub struct StallOutcome { pub stuck: bool, pub reason: Option<String> }

// src/team/daemon/decisions.rs state (TeamDaemon.decisions)
pub(crate) struct DecisionsState {
    pub jobs: HashMap<(DecisionPath, String), DecisionJob>,   // key = member or task/message id
    pub gate: decide::gate::ChangeGate,                        // last hash + last answer
    pub screen: HashMap<String, ScreenOutcome>,                // member -> latest
    pub triage: decide::gate::BoundedCache<u64, TriageOutcome>, // body hash -> verdict (256);
                                                                // HashMap + VecDeque, no new crate
    pub stall_reason: HashMap<String, String>,
    pub completion_veto: HashMap<u32, CompletionOutcome>,      // task id -> outcome
    pub pending_response: HashMap<String, String>,             // engineer -> redacted last response
    pub no_key_reported: bool,
    pub budget_reported_on: Option<chrono::NaiveDate>,
}
```

### List of tasks, in order

```yaml
Task 1 (config):
MODIFY src/team/config/types.rs: add DecisionProvider, DecisionPath, DecisionTimeouts, DecisionsConfig
  (mirror BenchRecoveryConfig 492-550); add `#[serde(default)] pub decisions: DecisionsConfig` to
  WorkflowPolicy and its Default.
MODIFY src/team/config/tests_advanced.rs: defaults + partial override tests.
MODIFY src/team/test_support.rs: builder keeps provider = Heuristic (explicit, as bench_recovery).

Task 2 (redaction):
CREATE src/team/decide/redact.rs (+ src/team/decide/mod.rs, `pub mod decide;` in src/team/mod.rs):
  patterns (OnceLock<Vec<(kind, Regex)>>): OpenAI/Anthropic keys (sk-[A-Za-z0-9_-]{20,}, sk-ant-...),
  GitHub (gh[pousr]_[A-Za-z0-9]{36,}, github_pat_...), Slack (xox[abprs]-...), AWS (AKIA[0-9A-Z]{16},
  aws_secret_access_key=...), Google (AIza[0-9A-Za-z_-]{35}), JWT (eyJ...\.eyJ...\.[...]), PEM blocks
  (-----BEGIN [A-Z ]*PRIVATE KEY-----...END), URL credentials (scheme://user:pass@), Bearer tokens,
  env-style secrets (^\s*(export\s+)?[A-Z0-9_]*(KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL)[A-Z0-9_]*\s*=\s*\S+),
  long high-entropy runs: hex of 48+ chars (so 40-char git SHAs are kept) and base64 of 40+ chars.
  redact(text) -> String replacing matches with [REDACTED:<kind>].
  TESTS: one per pattern; false positives (ordinary code, 40-char git SHAs, UUIDs are kept), idempotence.

Task 3 (client):
CREATE src/team/decide/client.rs:
  build_request(model, input, questions, zdr) -> serde_json::Value (pure)
  parse_response(&Value) -> Result<DecisionResponse> (pure; cost from provider_metadata.gateway.cost string)
  parse_error(status, body) -> Fallback (401/402/403/429/5xx -> Http(code))
  send(endpoint, key, body, timeout) -> Result<DecisionResponse, Fallback> using
    crate::team::http::agent().post(endpoint).timeout(timeout).set("Authorization", ..)
    .set("Content-Type","application/json").send_string(..); redact Transport/Status errors.
  TESTS: request shape for each question type; parse success/refusal/unknown answer/invalid JSON;
  error mapping; a TcpListener fake server for one round trip + a timeout case.

Task 4 (budget + gate):
CREATE src/team/decide/budget.rs: telemetry_db table decision_spend(date TEXT PRIMARY KEY, usd REAL,
  calls INTEGER) added in telemetry_db::init_schema; add_spend(conn, date, usd), spend_on(conn, date).
CREATE src/team/decide/gate.rs: ChangeGate keyed by (path,key) storing (hash, answer); bounded LRU for triage.
  hash = sha2 over the redacted input + the question set id.
  TESTS: spend upsert and read in open_in_memory; unchanged input hits the gate; LRU bound.

Task 5 (path builders + mappers):
CREATE src/team/decide/paths.rs and criteria.rs:
  screen_questions() + map -> ScreenOutcome (choice idle|working|context_exhausted|unknown)
  triage_questions() + map -> TriageOutcome
  completion_questions(task, response, diff_summary) + map -> CompletionOutcome (predicates done, narration_only)
  rubric_questions(criteria) + map -> RubricOutcome (score per criterion with levels
    ["not met","partly met","met"], predicate criteria_met)
  stall_questions() + map -> StallOutcome (predicate stuck + choice reason in a fixed list:
    waiting_on_input|blocked_on_dependency|looping|idle_without_work|other)
  apply_confidence(answer, min) -> Result<_, Fallback::LowConfidence>
  criteria::parse_acceptance_criteria(task_body) -> Vec<String> (bullets under "## Acceptance Criteria"; cap 10)
  input builders cap the redacted input at 96K chars (truncate head, keep tail for screens).
  TESTS: every mapper incl. refusal/unknown/low confidence; criteria parser with/without section.

Task 6 (daemon job plumbing):
CREATE src/team/daemon/decisions.rs impl TeamDaemon:
  fn decisions_enabled(path) -> bool (provider Jev && path in cfg.paths && key present; report NoKey once)
  fn start_decision(path, key, input, questions, timeout): redact -> gate check (hit => apply cached) ->
    budget check (spend_on(today) >= cap => Budget fallback, decision_budget_exceeded once/day) ->
    TestJob::spawn_with_timeout(label, Some(timeout), move |_| client::send(...)) stored in decisions.jobs.
  fn poll_decisions(&mut self): take finished jobs; on Ok: add_spend, emit decision_made, map via paths.rs,
    store in the per-path cache; on Err/TimedOut: emit decision_fallback, clear the pending flag.
MODIFY src/team/daemon.rs: `decisions: DecisionsState` field + constructor; `mod decisions;`.
MODIFY src/team/daemon/poll.rs: run_recoverable_step("poll_decisions", ..) before check_working_state_timeouts.
MODIFY src/team/events.rs: decision_made, decision_fallback, decision_budget_exceeded, completion_vetoed,
  review_rubric constructors + serialization tests.
  TESTS: disabled/no key/budget paths emit the right fallback and never spawn; a gate hit spawns nothing.

Task 7 (triage):
MODIFY src/team/delivery/routing.rs orchestrator_only_reason (761-784): for manager/architect recipients
  (uses_management_batching role check) when triage is enabled: look up decisions.triage by
  hash(redact(body)); Noise|StatusReply => Some(OrchestratorOnlyReason::JevSuppressed(kind)) (new
  variant, label "jev_noise"/"jev_status_reply"); Actionable|Escalation => None (Escalation: deliver via
  QueueTier::Priority when tiered inboxes are on; otherwise prefix "[escalation] "). Cache miss: live path
  (deliver_message_with_id) uses today's heuristic and starts a job; inbox path (deliver_inbox_messages)
  starts a job and skips the message this tick until the answer arrives or the triage timeout passes.
  TESTS (routing tests pattern 3955/4074/4107-4157): suppressed noise logs to the orchestrator; engineer
  recipients unaffected; a cache miss on the live path behaves as today.

Task 8 (completion veto):
MODIFY shim_events.rs on_completion: before 303 store redact(response) in decisions.pending_response.
MODIFY merge/completion/mod.rs: when completion is enabled, start the completion decision alongside the
  verification job (143-146) with task text + pending response + ctx.diff summary; in
  finish_engineer_completion (between 172 and 183) when has_required_evidence && passed and a veto
  outcome exists with !done or narration_only => reject_completion with an explicit Rejection
  (NarrationOnly if narration_only else InsufficientEvidence) and emit completion_vetoed. If the decision
  is still pending: verification path waits for it inside apply_verification_result up to the completion
  timeout; test-skipped path defers to the next tick, again up to the timeout. Fallback => today's path.
MODIFY gates.rs reject_completion: accept an explicit Rejection override (keep the existing signature for
  existing callers via a thin wrapper).
  TESTS (gates test fixtures): veto rejects a passing completion; veto never fires on NoCommits/TestsFailed;
  a Jev fallback leaves the outcome unchanged; timeout proceeds.

Task 9 (rubric):
MODIFY merge/completion/decision.rs route_to_manual_review and route_override_to_review: when rubric is
  enabled and criteria exist, start the rubric decision; when it finishes, queue a follow-up manager message
  "[rubric] Task #N: criteria met: yes|no\n- <criterion>: <score label>" and emit review_rubric. Never
  touch merge or bench decisions.
  TESTS: message text; no criteria => no job; rubric failure => no message, a fallback event.

Task 10 (screen state):
MODIFY src/team/daemon/agent_handle.rs: add `pty_runtime: bool` set at spawn (launcher.rs 189/392 via
  shim_spawn.rs 63-103).
MODIFY poll_shim.rs: for manager/architect handles with pty_runtime and screen_state enabled, send
  Command::CaptureScreen{last_n_lines: Some(100)} at most once per poll interval; handle
  Event::ScreenCapture at 306-309 (redact; never log content) -> start_decision(ScreenState, member, ..).
  On a ScreenOutcome that differs from the handle state, apply it to the daemon view only
  (apply_state_change + MemberState mapping as on_state_changed does).
  TESTS: socketpair mock: CaptureScreen sent only for PTY supervisors; SDK members and engineers never;
  ScreenCapture content never reaches logs; a verdict updates MemberState.

Task 11 (stall reason):
MODIFY stall.rs record_supervisory_stall_reason (572-622) / supervisory_progress_signal consumers: when
  the signal is SupervisoryProgress::None and stall is enabled, start a stall decision on the member's
  latest screen/output (redacted); when it finishes with stuck=true, include the reason in the
  stall_detected_with_reason event details and the supervisory stall summary. Never change Expected/Incidental.
  TESTS: None -> reason added; Expected/Incidental untouched; fallback leaves today's output.

Task 12 (docs):
MODIFY docs/config-reference.md (decisions bullet + field list + privacy note: what is sent, redaction,
  zero data retention), docs/migrating-to-0.12.md (opt-in feature), CHANGELOG.md (Added).
```

### Per-task pseudocode

```rust
// decide/client.rs
pub fn send(endpoint: &str, key: &str, body: &Value, timeout: Duration) -> Result<DecisionResponse, Fallback> {
    let result = crate::team::http::agent()
        .post(endpoint)
        .timeout(timeout)                                   // overall deadline (ureq 2)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
    match result {
        Ok(resp) => parse_response(&resp.into_json::<Value>().map_err(|_| Fallback::InvalidAnswer)?),
        Err(ureq::Error::Status(code, _resp)) => Err(Fallback::Http(code)),   // body not logged
        Err(ureq::Error::Transport(t)) if is_timeout(&t) => Err(Fallback::Timeout),
        Err(ureq::Error::Transport(_)) => Err(Fallback::Http(0)),
    }
}

// daemon/decisions.rs
fn start_decision(&mut self, path: DecisionPath, key: String, raw_input: &str, questions: Vec<Question>) {
    if !self.decisions_enabled(path) { return; }                 // emits NoKey once if applicable
    let input = decide::redact::redact(raw_input);
    if self.decisions.gate.unchanged(path, &key, &input, &questions) { return; } // cached answer stands
    if decide::budget::over_cap(self.telemetry_db.as_ref(), today_utc(), cfg.daily_spend_cap_usd) {
        self.report_budget_exceeded_once(); return;
    }
    let body = decide::client::build_request(&cfg.model, &input, &questions, cfg.zero_data_retention);
    let job = TestJob::spawn_with_timeout(format!("decide-{path:?}-{key}"), Some(timeout), move |_pgid| {
        decide::client::send(&endpoint, &api_key, &body, timeout).map_err(anyhow::Error::from)
    });
    self.decisions.jobs.insert((path, key), DecisionJob { job, started: Instant::now(), questions });
}
```

### Integration Points

```yaml
CONFIG:
  - add to: src/team/config/types.rs WorkflowPolicy.decisions (DecisionsConfig)
  - secret: API key only from env var `api_key_env` (default AI_GATEWAY_API_KEY); never in team.yaml
DAEMON LOOP:
  - add to: src/team/daemon/poll.rs tick(): run_recoverable_step("poll_decisions") before 276
DATABASE:
  - table: decision_spend(date TEXT PRIMARY KEY, usd REAL NOT NULL DEFAULT 0, calls INTEGER NOT NULL DEFAULT 0)
    in telemetry_db::init_schema (CREATE TABLE IF NOT EXISTS); upsert ON CONFLICT(date) DO UPDATE
EXTERNAL API:
  - POST https://ai-gateway.vercel.sh/v1/decisions (configurable endpoint for tests)
PROTOCOL:
  - reuse Command::CaptureScreen / Event::ScreenCapture (no protocol change)
FRONTEND / ROUTES: not applicable
```

## Validation Loop

### Level 1: Syntax & Style

```bash
export CARGO_TARGET_DIR=target   # or a shared target dir
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

### Level 2: Unit tests

```bash
cargo test --lib decide            # redaction, client, budget, gate, paths, criteria
cargo test --lib config            # defaults
cargo test --lib routing           # triage
cargo test --lib completion        # veto
cargo test --lib stall             # stall reason
cargo test --lib                   # full suite
```

### Level 3: Integration and live

```bash
env -u TMUX TMUX_TMPDIR=$(mktemp -d) cargo test --lib --features integration
# Live smoke (manual; needs a key):
AI_GATEWAY_API_KEY=... curl -sS -X POST https://ai-gateway.vercel.sh/v1/decisions \
  -H "Authorization: Bearer $AI_GATEWAY_API_KEY" -H "Content-Type: application/json" \
  -d '{"model":"typesafe-ai/jev-1.14.0","input":"hello","questions":[{"type":"predicate","name":"greeting","instructions":"Is this a greeting?"}],"providerOptions":{"gateway":{"zeroDataRetention":true}}}'
```

## Final validation Checklist

- [ ] All tests pass: `cargo test` and the integration suite on a private tmux server.
- [ ] No linting errors: `make lint`.
- [ ] Build succeeds: `cargo build`.
- [ ] Manual test: with a real key on a scratch project, set `provider: jev`, then:
  - send a noise message to the manager and check it is suppressed with a `decision_made` event;
  - complete a narration-only task and check `completion_vetoed`;
  - check spend appears in `telemetry.db` `decision_spend`.
- [ ] Error cases: no key, 401, 402 quota, 429, 5xx, timeout, refusal, invalid JSON. Each falls back.
- [ ] No raw content or key in `events.jsonl`, the orchestrator log, the shim event log or tracing output. Grep for a planted secret in a test.
- [ ] Docs updated: config reference (with privacy note), migration guide, changelog.
- [ ] No new dependencies (regex, sha2, ureq, serde_json and rusqlite already exist).

---

## Anti-Patterns to Avoid

- ❌ Calling the Gateway on the poll thread or inside routing, stall or gate functions.
- ❌ Logging raw pane content, message bodies, diffs or the API key.
- ❌ Letting Jev accept a completion that a hard gate rejected, or block a completion when Jev fails.
- ❌ Using `/v1/chat/completions` or the deprecated `/v1/evaluate`.
- ❌ Floating-point parsing of `cost` from a JSON number; it is a string.
- ❌ An unbounded change-gate cache.
- ❌ Turning the feature on by default or reading the key from `team.yaml`.

## Confidence

**6/10** for a one-pass implementation. The pieces are well mapped: client, redaction, budget, gate and mappers are pure and testable. The integration points have exact lines. The risks are concentrated in three places:
- the completion veto's join with the verification job and the test-skipped path;
- the inbox-deferral path in routing;
- daemon-side screen state alongside the shim's own state.

Build Tasks 1–6 first. Then add one path at a time, in the order triage, completion, rubric, stall, screen. Each path is independently switchable through `paths`.
