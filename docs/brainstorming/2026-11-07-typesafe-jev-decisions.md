# Feature Brainstorming Session: TypeSafe AI Jev for supervisory decisions

**Date:** 2026-11-07
**Session Type:** Feature Planning / Technical Design

## 1. Context & Problem Statement

### Problem Description

Batty makes many yes/no and choose-one decisions with heuristics: regexes, keyword lists, commit counts and screen-pattern matching. These heuristics are brittle. They cause false parks, missed narration, misrouted messages and empty completions that slip through.

[Jev](https://vercel.com/ai-gateway/models/jev) is TypeSafe AI's "System One decision model", served through the Vercel AI Gateway as `typesafe-ai/jev` (latest `typesafe-ai/jev-1.14.0`). It evaluates a block of state text against typed questions (`boolean`, choice, score) and returns typed answers. Several questions can be asked in parallel in one request. It costs $0.04 per 1M input tokens, with a 32K context. Its stated uses are classification, routing, rubric assessment and automated verification. That is a close match for batty's decision points.

The page's example (TypeScript AI SDK):

```ts
import { experimental_decide as decide } from 'ai';

const result = await decide({
  model: 'typesafe-ai/jev',
  state: 'The support agent issued a full refund to the customer.',
  questions: {
    refunded: { type: 'boolean', instructions: 'Was a refund issued?' },
  },
});
```

### Target Users

- **Primary users:** operators running batty teams. Supervisory decisions get more accurate, with fewer stalls, false parks and misrouted messages.
- **Secondary users:** manager and architect agents, which receive better-filtered inboxes and better-founded completion and review signals.

### Success Criteria

- **Business metrics:** fewer human interventions per completed task. Track this with existing telemetry: `task_escalated`, unbench actions and manual merges.
- **User metrics:** fewer unattributed or empty completions reaching merge, and fewer supervisory stalls.
- **Technical metrics:** p95 decision latency under the per-path timeout. Spend per project per day under the cap. The heuristic fallback takes over within one poll when the Gateway is unavailable.

### Constraints & Assumptions

- **Technical constraints:**
  - Batty is Rust and the official client is the TypeScript AI SDK, so batty calls the Gateway over HTTP. `ureq` is already a dependency.
  - The daemon poll loop must never block, so calls run off the poll thread.
  - The context limit is 32K tokens.
- **Business constraints:**
  - Spend cap of $5 per day per project by default.
  - A call is made only when its redacted input changed since the last answer.
- **Regulatory/compliance:**
  - Content (task text, agent output, diffs, messages) is sent to the Gateway and its providers (TypeSafe AI, US; DigitalOcean).
  - Keys, tokens, secrets and `.env`-style values are redacted before sending.
  - Zero-data-retention or no-training routing is not required, by decision.
- **Assumptions:**
  - The Gateway exposes the decide capability over an HTTP API batty can call with an `AI_GATEWAY_API_KEY`. The exact endpoint and request shape still need checking against the docs; see questions.
  - Screen state and stall detection for the manager and architect are in scope, consistent with "only for manager and architect".

## 2. Brainstormed Ideas & Options

### Option A: Advisory (shadow mode)

- **Description:** heuristics keep deciding. Jev runs alongside, and its verdicts are logged and recorded as events, with a disagreement report.
- **Key features:** shadow calls, a disagreement log, an accuracy report.
- **Pros:** no risk to behaviour; it measures Jev before trusting it.
- **Cons:** no direct benefit until a later switch; it costs money for data alone.
- **Effort estimate:** S
- **Risk level:** Low
- **Dependencies:** the Gateway client and redaction.

### Option B: Tiebreaker

- **Description:** heuristics decide clear cases, and Jev decides only the ambiguous ones (for example, low classifier confidence).
- **Key features:** confidence thresholds per path.
- **Pros:** the fewest calls and the lowest cost.
- **Cons:** needs a trustworthy "ambiguous" signal from each heuristic, and many heuristics have none.
- **Effort estimate:** M
- **Risk level:** Medium
- **Dependencies:** confidence scores for every heuristic.

### Option C: Primary with heuristic fallback (chosen)

- **Description:** Jev decides on the in-scope paths. Today's heuristic is the fallback when the Gateway is unreachable, times out, is over budget, or when Jev is disabled.
- **Key features:** a decision trait with Jev and heuristic implementations, change-gated calls, timeouts, a spend cap, redaction, and an event per decision.
- **Pros:** the accuracy gain where it matters most; heuristics remain as a safety net.
- **Cons:** network dependency on supervisory paths; misclassification risk; content leaves the machine.
- **Effort estimate:** L
- **Risk level:** Medium
- **Dependencies:** the Gateway API, redaction, budget tracking.

### Additional Ideas Considered

- Engineer screen-state and stall detection through Jev: deferred. It is the hot path, and the cost and latency are not justified yet.
- Exposing Jev as a tool the manager or architect agents can call themselves: not chosen.
- Shadow mode for engineer paths later, to collect accuracy data at low risk.

## 3. Decision Outcome

### Chosen Approach

**Selected solution:** Option C, primary with heuristic fallback, for supervisory decisions only.

In scope (Jev decides):

1. **Completion checks:** the manager's acceptance call on an engineer completion. Was the task actually done, or was it narration or an empty completion? Inputs: task text, the agent's final message, and a diff summary.
1. **Review rubric:** score a `review` task against its acceptance criteria. This feeds the manager review and the bench-recovery bar.
1. **Inbox triage for the manager and architect:** actionable, status reply, noise or escalation. This replaces keyword routing for those recipients.
1. **Screen state for the manager and architect:** idle, working or waiting for input.
1. **Stall and blocker detection for the manager and architect:** stuck or not, with the reason.

Out of scope: engineer screen state, engineer stall detection and engineer inbox routing, which stay on the heuristics.

### Rationale

- The supervisory paths gate merges and routing for the whole team, so accuracy there has the highest leverage.
- Restricting Jev to two members keeps the screen-state hot path cheap: about 2 panes, not N.
- A heuristic fallback keeps batty working offline and when the budget runs out.

### Trade-offs Accepted

- **Gaining:** structured, typed decisions on the paths with the highest leverage.
- **Sacrificing:** offline-only decisions on those paths. Redacted content is sent to a third party. The model can be wrong where a heuristic would have been right.
- **Future considerations:** shadow-mode engineer paths; per-path accuracy telemetry to tune or roll back.

## 4. Implementation Plan

### MVP Scope (Phase 1)

**Core features for initial release:**

- [ ] `src/team/decide/` module: a `Decider` trait (`decide(state, questions) -> Answers`) with a `JevDecider` (Gateway HTTP via `ureq`) and a `HeuristicDecider` (wraps today's code).
- [ ] Typed questions: `Boolean`, `Choice { options }` and `Score { min, max }`, with serde types matching the Gateway API.
- [ ] Redaction before every call: key and token patterns (`sk-…`, `ghp_…`, `xox…-`, AWS keys, JWTs, long base64/hex), `KEY=value` lines from env-style files, PEM blocks, and URL credentials. The redacted text is what gets hashed and sent.
- [ ] Change gate: per (path, member) hash of the redacted input. An unchanged input reuses the last answer, with no call.
- [ ] Budget: track estimated tokens and spend per project per day in telemetry. Over `daily_spend_cap_usd`, fall back to heuristics and alert the human once.
- [ ] Timeouts: 2 s for screen state and triage, 10 s for completion and rubric. On timeout or error, use the heuristic and record a `decision_fallback` event.
- [ ] Off the poll thread: calls run in background jobs (the `TestJob` pattern); the poll loop reads finished answers.
- [ ] Integration:
  - completion checks (`team/merge/completion/gates.rs`, `daemon/completion_checks.rs`)
  - review rubric (`daemon/automation/reviews.rs`, bench-recovery bar)
  - manager and architect inbox triage (`delivery/routing.rs` `orchestrator_only_reason`)
  - manager and architect screen state (`shim/classifier.rs` `classify_with_confidence` call sites)
  - stall detection (`daemon/health/stall.rs`, `narration.rs`)
- [ ] Config `workflow_policy.decisions`:

```yaml
workflow_policy:
  decisions:
    provider: jev                 # jev | heuristic
    model: typesafe-ai/jev
    api_key_env: AI_GATEWAY_API_KEY
    daily_spend_cap_usd: 5.0
    timeouts_ms: { screen_state: 2000, triage: 2000, stall: 2000, completion: 10000, rubric: 10000 }
    paths: [completion, rubric, triage, screen_state, stall]
    roles: [manager, architect]   # members whose screen/stall/inbox use Jev
```

- [ ] Events: `decision_made` (path, member, provider, latency_ms, cached), `decision_fallback` (reason), and `decision_budget_exceeded`.

**Acceptance criteria:**

- As an operator, I can set `workflow_policy.decisions.provider: jev` with an API key, so that completion checks, rubric scoring, triage, and manager/architect screen and stall decisions use Jev.
- Given no key, an unreachable Gateway or an exhausted budget, every path falls back to today's heuristic in the same poll, and a `decision_fallback` event says why.
- Given an unchanged redacted input, no Gateway call is made.
- Given content with a known secret pattern, the sent payload contains a redaction marker, not the secret. A unit test asserts this for every pattern.
- Engineer screen and stall decisions never call Jev.

**Definition of Done:**

- [ ] Feature implemented and tested: unit tests for redaction, change gating, budget, fallback, and question/answer serde, plus a fake Gateway HTTP server in tests.
- [ ] Code reviewed and merged.
- [ ] Documentation updated: config reference, a privacy note, the migration guide and the changelog.
- [ ] Performance criteria met: the poll loop never blocks, and timeouts are honoured.
- [ ] Security requirements satisfied: redaction is tested; the key comes only from the environment and is never logged.
- [ ] User acceptance testing passed: a live run with a real Gateway key on a scratch project.

### Future Enhancements (Phase 2+)

- Shadow mode for engineer paths, to gather an accuracy comparison before extending Jev further.
- Per-path accuracy telemetry and Grafana panels (agreement rate with heuristics, fallback rate, spend).
- A switch for zero-data-retention or regional routing if policy changes later.

**Nice-to-have improvements:**

- Batching several questions per member into one request, which Jev supports.
- A local cache of recent answers that survives a daemon restart.

## 5. Action Items & Next Steps

### Immediate Actions (This Week)

- [ ] **Confirm the Gateway decide API over HTTP**
  - **Dependencies:** Vercel AI Gateway docs and an API key.
  - **Success criteria:** the exact endpoint, request and response JSON, and auth header are documented, with a working `curl` example for `typesafe-ai/jev`.
- [ ] **Write the redaction module and its tests first**
  - **Dependencies:** none.
  - **Success criteria:** a pattern list with a test case each, and false-positive checks on ordinary code.

### Short-term Actions (Next Sprint)

- [ ] **Run `/bp:generate-prp` on this brainstorm** for file-level design and a task breakdown.
- [ ] **Implement the `Decider` trait, `JevDecider`, the fallback, the budget and the change gate, then integrate the paths one at a time**, starting with completion checks.

## 6. Risks & Dependencies

### Technical Risks

- **Risk:** the Gateway decide endpoint is only exposed through the TypeScript SDK, or is experimental and changes.
  - **Impact:** High
  - **Probability:** Medium
  - **Mitigation:** confirm the HTTP contract first. Keep the client behind the `Decider` trait. Pin the model version (`jev-1.14.0`). Fall back to heuristics on any schema error.
- **Risk:** a secret leaks through redaction.
  - **Impact:** High
  - **Probability:** Low
  - **Mitigation:** a conservative pattern set; redact before hashing and logging; tests per pattern; never send `.env` files or files matched by `.gitignore` secrets patterns.
- **Risk:** misclassification on a gating path (for example, completion accepted wrongly).
  - **Impact:** Medium
  - **Probability:** Medium
  - **Mitigation:** keep verification tests and the merge gates as independent checks; record every decision as an event so it can be audited; per-path disable switch.
- **Risk:** spend or latency spikes.
  - **Impact:** Medium
  - **Probability:** Low
  - **Mitigation:** the change gate, a daily cap with fallback, and timeouts; scope limited to two members for screen and stall.
- **Risk:** the 32K context is exceeded by large diffs.
  - **Impact:** Low
  - **Probability:** Medium
  - **Mitigation:** send a diff summary (files, stat, truncated hunks) within a token budget, not the full diff.

## 7. Resources & References

### Technical Documentation

- <https://vercel.com/ai-gateway/models/jev> - model page: capabilities, pricing, providers, SDK example.
- Vercel AI Gateway docs (linked from the model page as "Read the docs") - HTTP API and auth; to be confirmed.

### Codebase References

- `src/shim/classifier.rs` (`classify`, `classify_with_confidence`, `classify_narration_line`) - screen-state heuristics to wrap.
- `src/team/delivery/routing.rs` (`orchestrator_only_reason`, `is_status_query`, `is_engineer_ambient_chatter`) - triage heuristics.
- `src/team/daemon/health/stall.rs`, `src/team/daemon/health/narration.rs` - stall and narration detection.
- `src/team/merge/completion/gates.rs`, `src/team/daemon/completion_checks.rs` - completion acceptance.
- `src/team/daemon/automation/reviews.rs`, `src/team/daemon/bench_recovery.rs` - review handling and the bench-recovery bar.
- `src/team/daemon/background_tests.rs` (`TestJob`) - pattern for off-thread work.
- `ureq` (`Cargo.toml`) - existing HTTP client.

## 8. Session Notes & Insights

### Key Insights Discovered

- Jev's typed-question model maps directly onto batty's existing heuristic decision points, so it can sit behind one trait with the heuristics as the fallback.
- Limiting Jev to supervisory decisions and two members keeps cost low while covering the paths that gate the whole team.
- Gating calls on a change in the redacted input makes cost proportional to activity, not to poll frequency.

### Questions Raised (For Future Investigation)

- What is the exact HTTP contract for `decide` on the AI Gateway, and is it stable or experimental?
- Do the per-path timeouts (2 s / 10 s) hold under real Gateway latency?
- Should rubric scores feed the bench-recovery bar directly, or stay advisory there?

### Team Feedback

- Owner decisions:
  - all five paths;
  - primary authority with heuristic fallback;
  - send content with secrets redacted;
  - call only on changed input;
  - supervisory decisions only (manager and architect).
- Timeouts and the spend cap use the facilitator's suggested defaults, because the owner did not specify them.
