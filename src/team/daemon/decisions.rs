//! Daemon plumbing for Jev decisions.
//!
//! Every Gateway call runs as a background [`TestJob`]; the poll thread only
//! starts jobs and, on later ticks, takes finished ones. Synchronous callers
//! (routing, stall detection, completion gates) read cached outcomes through
//! [`TeamDaemon::cached_decision`] and [`TeamDaemon::cached_triage`] and fall
//! back to their heuristic on a miss.
//!
//! Redaction happens before hashing, sending and storing. Events carry only
//! fixed labels, never content or the API key.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::debug;

use super::TeamDaemon;
use crate::shim::protocol::ShimState;
use crate::team::config::RoleType;
use crate::team::config::{DecisionPath, DecisionProvider};
use crate::team::decide::gate::{self, BoundedCache, ChangeGate};
use crate::team::decide::paths::{
    self, CompletionOutcome, ScreenOutcome, StallOutcome, TriageOutcome,
};
use crate::team::decide::{
    Answer, DecisionOutcome, DecisionResponse, Fallback, Question, budget, client, redact,
};
use crate::team::events::TeamEvent;
use crate::team::standup::MemberState;
use crate::team::test_job::{TestJob, TestTimedOut};

/// A failed decision is not retried for the same input until this passes, so
/// an outage cannot turn every poll into a request.
const FAILURE_RETRY_COOLDOWN: Duration = Duration::from_secs(60);

type JobKey = (DecisionPath, String);

struct DecisionJob {
    job: TestJob<DecisionResponse>,
    started: Instant,
    /// Hash of the redacted input plus the question set (change gate).
    gate_hash: u64,
    /// Hash of the redacted input alone (triage cache key).
    triage_key: u64,
    /// Redacted criteria a rubric answer maps onto.
    criteria: Vec<String>,
}

pub(crate) struct DecisionsState {
    /// Calls in flight, one per `(path, key)`.
    jobs: HashMap<JobKey, DecisionJob>,
    /// Latest outcome per `(path, key)` for every path except triage.
    outcomes: ChangeGate<DecisionOutcome>,
    /// Triage verdicts keyed by the hash of the redacted body.
    triage: BoundedCache<u64, TriageOutcome>,
    /// Inputs that failed recently: `(path, key)` to `(gate hash, when)`.
    failed: BoundedCache<JobKey, (u64, Instant)>,
    /// Inbox message ids held back while their triage call runs, with the
    /// time each was first held.
    triage_deferred: BoundedCache<String, Instant>,
    /// Redacted last response per engineer, kept for the completion veto.
    pending_response: HashMap<String, String>,
    /// Manager to tell about each rubric in flight, keyed by task id.
    rubric_recipients: BoundedCache<String, String>,
    /// Supervisory stalls held back while their stall call runs, with the
    /// time each was first held.
    stall_deferred: BoundedCache<String, Instant>,
    /// Latest redacted screen capture per supervisor.
    latest_screen: HashMap<String, String>,
    no_key_reported: bool,
    budget_reported_on: Option<String>,
    /// Spend since startup as `(UTC date, USD)`, a floor when the telemetry
    /// database is unavailable.
    session_spend: Option<(String, f64)>,
}

impl Default for DecisionsState {
    fn default() -> Self {
        Self {
            jobs: HashMap::new(),
            outcomes: ChangeGate::default(),
            triage: BoundedCache::new(gate::TRIAGE_CACHE_CAPACITY),
            failed: BoundedCache::new(gate::GATE_CAPACITY),
            triage_deferred: BoundedCache::new(gate::TRIAGE_CACHE_CAPACITY),
            pending_response: HashMap::new(),
            rubric_recipients: BoundedCache::new(gate::GATE_CAPACITY),
            stall_deferred: BoundedCache::new(gate::GATE_CAPACITY),
            latest_screen: HashMap::new(),
            no_key_reported: false,
            budget_reported_on: None,
            session_spend: None,
        }
    }
}

fn map_outcome(
    path: DecisionPath,
    answers: &[Answer],
    criteria: &[String],
    min_confidence: f64,
) -> Result<DecisionOutcome, Fallback> {
    Ok(match path {
        DecisionPath::ScreenState => {
            DecisionOutcome::Screen(paths::map_screen(answers, min_confidence)?)
        }
        DecisionPath::Triage => {
            DecisionOutcome::Triage(paths::map_triage(answers, min_confidence)?)
        }
        DecisionPath::Completion => {
            DecisionOutcome::Completion(paths::map_completion(answers, min_confidence)?)
        }
        DecisionPath::Rubric => {
            DecisionOutcome::Rubric(paths::map_rubric(answers, criteria, min_confidence)?)
        }
        DecisionPath::Stall => DecisionOutcome::Stall(paths::map_stall(answers, min_confidence)?),
    })
}

/// The triage input for a message body: redacted and capped.
fn triage_input_of(body: &str) -> String {
    paths::triage_input(&redact::redact(body))
}

/// The triage cache key for a message body. Matches the key
/// [`TeamDaemon::spawn_decision`] derives from [`triage_input_of`].
pub(crate) fn triage_body_hash(body: &str) -> u64 {
    gate::text_hash(&redact::redact(&triage_input_of(body)))
}

/// Bytes read from the end of a member's PTY log for the stall decision.
const STALL_OUTPUT_TAIL_BYTES: u64 = 64 * 1024;

/// The tail of a PTY log as plain text (ANSI stripped), or empty when the
/// log is missing.
fn pty_log_tail(path: &std::path::Path) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(STALL_OUTPUT_TAIL_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    crate::prompt::strip_ansi(&String::from_utf8_lossy(&bytes))
}

/// The follow-up manager message for a rubric outcome.
fn rubric_message(task_key: &str, outcome: &paths::RubricOutcome) -> String {
    let mut message = format!(
        "[rubric] Task #{task_key}: criteria met: {}",
        if outcome.criteria_met { "yes" } else { "no" }
    );
    for (criterion, score) in &outcome.scores {
        message.push_str(&format!("\n- {criterion}: {}", paths::rubric_label(*score)));
    }
    message
}

fn fallback_of(error: &anyhow::Error) -> Fallback {
    if let Some(fallback) = error.downcast_ref::<Fallback>() {
        *fallback
    } else if error.downcast_ref::<TestTimedOut>().is_some() {
        Fallback::Timeout
    } else {
        Fallback::Http(0)
    }
}

impl TeamDaemon {
    fn decision_api_key(&self) -> Option<String> {
        let name = &self
            .config
            .team_config
            .workflow_policy
            .decisions
            .api_key_env;
        std::env::var(name)
            .ok()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
    }

    /// True when `path` may call Jev: provider `jev`, the path listed, and an
    /// API key in the environment. A missing key is reported once with a
    /// `decision_fallback` event, then stays silent.
    pub fn decisions_enabled(&mut self, path: DecisionPath) -> bool {
        let policy = &self.config.team_config.workflow_policy.decisions;
        if policy.provider != DecisionProvider::Jev || !policy.paths.contains(&path) {
            return false;
        }
        if self.decision_api_key().is_some() {
            return true;
        }
        if !self.decisions.no_key_reported {
            self.decisions.no_key_reported = true;
            self.emit_event(TeamEvent::decision_fallback(
                path.label(),
                None,
                &Fallback::NoKey.to_string(),
            ));
        }
        false
    }

    /// True when `path` is switched on in the config. Read-only, for
    /// synchronous callers that only consult cached outcomes; it does not
    /// check the key (a missing key leaves every cache empty).
    pub fn decision_path_configured(&self, path: DecisionPath) -> bool {
        let policy = &self.config.team_config.workflow_policy.decisions;
        policy.provider == DecisionProvider::Jev && policy.paths.contains(&path)
    }

    /// The cached triage verdict for a message body, if triage is on.
    pub fn triage_verdict(&self, body: &str) -> Option<TriageOutcome> {
        if !self.decision_path_configured(DecisionPath::Triage) {
            return None;
        }
        self.cached_triage(triage_body_hash(body))
    }

    /// Start a triage call for a message body. No-op when triage is off, the
    /// verdict is cached or a call for the same body is in flight.
    pub fn start_triage_decision(&mut self, body: &str) {
        if !self.decision_path_configured(DecisionPath::Triage) {
            return;
        }
        self.start_decision(
            DecisionPath::Triage,
            "",
            &triage_input_of(body),
            paths::triage_questions(),
        );
    }

    fn triage_in_flight(&self, body: &str) -> bool {
        let key = format!("{:016x}", triage_body_hash(body));
        self.decisions
            .jobs
            .contains_key(&(DecisionPath::Triage, key))
    }

    /// Inbox path: true when message `id` should wait this tick for its
    /// triage verdict. Starts the call on a miss. A message is never held
    /// once a verdict exists, no call is running (disabled, no key, budget,
    /// recent failure) or the triage timeout has passed since it was first
    /// held; the caller then decides with the heuristic.
    pub fn defer_for_triage(&mut self, id: &str, body: &str) -> bool {
        if !self.decision_path_configured(DecisionPath::Triage) {
            return false;
        }
        let id = id.to_string();
        if self.triage_verdict(body).is_some() {
            self.decisions.triage_deferred.remove(&id);
            return false;
        }
        self.start_triage_decision(body);
        if !self.triage_in_flight(body) {
            self.decisions.triage_deferred.remove(&id);
            return false;
        }
        let timeout = Duration::from_millis(
            self.config
                .team_config
                .workflow_policy
                .decisions
                .timeouts_ms
                .for_path(DecisionPath::Triage),
        );
        let since = match self.decisions.triage_deferred.peek(&id) {
            Some(since) => *since,
            None => {
                let now = Instant::now();
                self.decisions.triage_deferred.insert(id.clone(), now);
                now
            }
        };
        if since.elapsed() < timeout {
            return true;
        }
        self.decisions.triage_deferred.remove(&id);
        false
    }

    /// Test hook: store a triage verdict as if a call had answered.
    #[cfg(test)]
    pub(crate) fn seed_triage_verdict(&mut self, body: &str, verdict: TriageOutcome) {
        self.decisions
            .triage
            .insert(triage_body_hash(body), verdict);
    }

    /// Keep the engineer's final response (redacted, capped) for the
    /// completion veto. No-op unless the completion path is configured.
    pub fn stash_completion_response(&mut self, engineer: &str, response: &str) {
        if !self.decision_path_configured(DecisionPath::Completion) {
            return;
        }
        let redacted = paths::cap_input(&redact::redact(response), paths::Keep::Tail);
        self.decisions
            .pending_response
            .insert(engineer.to_string(), redacted);
    }

    /// Start the completion veto decision for `task_id` from the task text,
    /// the engineer's stashed response and a diff summary. A cached outcome
    /// for different input is dropped first, so a cached outcome always
    /// describes the current completion.
    pub fn start_completion_decision(
        &mut self,
        engineer: &str,
        task_id: u32,
        task_text: &str,
        diff_summary: &str,
    ) {
        let response = self
            .decisions
            .pending_response
            .remove(engineer)
            .unwrap_or_default();
        if !self.decisions_enabled(DecisionPath::Completion) {
            return;
        }
        let input = paths::completion_input(
            &redact::redact(task_text),
            &response,
            &redact::redact(diff_summary),
        );
        let questions = paths::completion_questions();
        let key = task_id.to_string();
        let hash = gate::input_hash(&redact::redact(&input), &questions);
        if !self
            .decisions
            .outcomes
            .unchanged(DecisionPath::Completion, &key, hash)
        {
            self.decisions
                .outcomes
                .forget(DecisionPath::Completion, &key);
        }
        self.start_decision(DecisionPath::Completion, &key, &input, questions);
    }

    /// True while the completion decision for `task_id` is in flight.
    pub fn completion_decision_pending(&self, task_id: u32) -> bool {
        self.decisions
            .jobs
            .contains_key(&(DecisionPath::Completion, task_id.to_string()))
    }

    /// The cached completion outcome for `task_id` when it vetoes the
    /// completion. Read-only; a miss means "accept as today".
    pub fn completion_veto(&self, task_id: u32) -> Option<CompletionOutcome> {
        if !self.decision_path_configured(DecisionPath::Completion) {
            return None;
        }
        match self.cached_decision(DecisionPath::Completion, &task_id.to_string()) {
            Some(DecisionOutcome::Completion(outcome)) if outcome.vetoes() => Some(*outcome),
            _ => None,
        }
    }

    /// Start the advisory review rubric for `task_id`; `manager` receives the
    /// result as a follow-up message once the call answers.
    pub fn start_review_rubric_decision(
        &mut self,
        task_id: u32,
        manager: &str,
        task_text: &str,
        diff_summary: &str,
        criteria: &[String],
    ) {
        if criteria.is_empty() || !self.decisions_enabled(DecisionPath::Rubric) {
            return;
        }
        let key = task_id.to_string();
        let input = paths::rubric_input(&redact::redact(task_text), &redact::redact(diff_summary));
        self.start_rubric_decision(&key, &input, criteria);
        if self
            .decisions
            .jobs
            .contains_key(&(DecisionPath::Rubric, key.clone()))
        {
            self.decisions
                .rubric_recipients
                .insert(key, manager.to_string());
        }
    }

    /// Queue the follow-up rubric message for a finished rubric call.
    fn deliver_rubric(&mut self, task_key: &str) {
        let Some(manager) = self
            .decisions
            .rubric_recipients
            .remove(&task_key.to_string())
        else {
            return;
        };
        let Some(DecisionOutcome::Rubric(outcome)) = self
            .cached_decision(DecisionPath::Rubric, task_key)
            .cloned()
        else {
            return;
        };
        let scores: Vec<f64> = outcome.scores.iter().map(|(_, score)| *score).collect();
        self.emit_event(TeamEvent::review_rubric(
            task_key,
            outcome.criteria_met,
            &scores,
        ));
        let message = rubric_message(task_key, &outcome);
        if let Err(error) = self.queue_message("daemon", &manager, &message) {
            debug!(task = task_key, error = %error, "failed to queue rubric message");
        }
    }

    /// The member's latest output for the stall decision: its latest screen
    /// capture, else the tail of its PTY log. Redacted by the caller.
    fn latest_member_output(&self, member: &str) -> String {
        if let Some(screen) = self.decisions.latest_screen.get(member) {
            return screen.clone();
        }
        pty_log_tail(&crate::team::shim_log_path(
            &self.config.project_root,
            member,
        ))
    }

    fn is_supervisor(&self, member: &str) -> bool {
        self.config
            .members
            .iter()
            .find(|m| m.name == member)
            .is_some_and(|m| matches!(m.role_type, RoleType::Architect | RoleType::Manager))
    }

    /// True when screen state may be classified for `member`: a manager or
    /// architect whose live handle runs on the PTY runtime.
    fn screen_state_applies(&self, member: &str) -> bool {
        self.is_supervisor(member)
            && self
                .shim_handles
                .get(member)
                .is_some_and(|handle| handle.pty_runtime && !handle.is_terminal())
    }

    /// Poll step: ask each PTY supervisor for its screen, at most once per
    /// poll interval. No-op unless screen state is enabled.
    pub(crate) fn request_supervisor_screens(&mut self) {
        if !self.decision_path_configured(DecisionPath::ScreenState)
            || !self.decisions_enabled(DecisionPath::ScreenState)
        {
            return;
        }
        let interval = self.poll_interval;
        let due: Vec<String> = self
            .shim_handles
            .iter()
            .filter(|(_, handle)| {
                handle
                    .last_screen_capture_at
                    .is_none_or(|at| at.elapsed() >= interval)
            })
            .map(|(name, _)| name.clone())
            .filter(|name| self.screen_state_applies(name))
            .collect();
        for name in due {
            if let Some(handle) = self.shim_handles.get_mut(&name)
                && let Err(error) = handle.request_screen_capture(paths::SCREEN_TAIL_LINES)
            {
                debug!(member = %name, error = %error, "screen capture request failed");
            }
        }
    }

    /// A screen capture arrived: redact it, keep it as the member's latest
    /// output, and classify it. The content is never logged.
    pub(crate) fn on_screen_capture(&mut self, member: &str, content: &str) {
        if !self.decision_path_configured(DecisionPath::ScreenState)
            || !self.screen_state_applies(member)
        {
            return;
        }
        let screen = paths::screen_input(&redact::redact(content));
        self.decisions
            .latest_screen
            .insert(member.to_string(), screen.clone());
        self.start_decision(
            DecisionPath::ScreenState,
            member,
            &screen,
            paths::screen_questions(),
        );
    }

    /// Apply a fresh screen verdict to the daemon's view of `member` (the
    /// handle state and `MemberState`), the way a shim state change maps.
    /// The shim's own state machine is untouched. Only idle and working are
    /// applied: a context-exhausted or unknown verdict is left to the shim's
    /// classifier, since a terminal state would stop delivery on a guess.
    fn apply_screen_outcome(&mut self, member: &str) {
        if !self.screen_state_applies(member) {
            return;
        }
        let Some(DecisionOutcome::Screen(outcome)) = self
            .cached_decision(DecisionPath::ScreenState, member)
            .cloned()
        else {
            return;
        };
        let (to, member_state) = match outcome {
            ScreenOutcome::Idle => (ShimState::Idle, MemberState::Idle),
            ScreenOutcome::Working => (ShimState::Working, MemberState::Working),
            ScreenOutcome::ContextExhausted | ScreenOutcome::Unknown => return,
        };
        let Some(handle) = self.shim_handles.get_mut(member) else {
            return;
        };
        if handle.state == to {
            return;
        }
        debug!(member, from = %handle.state, to = %to, "applying Jev screen state");
        handle.apply_state_change(to);
        if self.states.get(member).copied() != Some(member_state) {
            self.states.insert(member.to_string(), member_state);
            self.update_automation_timers_for_state(member, member_state);
            if member_state == MemberState::Working {
                self.health
                    .working_since
                    .entry(member.to_string())
                    .or_insert_with(Instant::now);
            } else {
                self.health.working_since.remove(member);
            }
        }
    }

    /// Supervisory stall with no heuristic signal: true when the stall
    /// report should wait this tick for a Jev reason. Starts the call on a
    /// miss. Never holds a stall once a current answer exists, no call is
    /// running (disabled, no key, budget, recent failure, no output) or the
    /// stall timeout has passed since it was first held. A Jev "not stalled"
    /// answer never suppresses the stall; it only adds no reason.
    pub fn defer_stall_for_jev(&mut self, member: &str) -> bool {
        if !self.decision_path_configured(DecisionPath::Stall) {
            return false;
        }
        let output = redact::redact(&self.latest_member_output(member));
        if output.trim().is_empty() {
            return false;
        }
        let input = paths::stall_input(&output);
        let questions = paths::stall_questions();
        let hash = gate::input_hash(&redact::redact(&input), &questions);
        let key = member.to_string();
        if self
            .decisions
            .outcomes
            .unchanged(DecisionPath::Stall, member, hash)
        {
            self.decisions.stall_deferred.remove(&key);
            return false;
        }
        // An answer about older output must not explain this stall.
        self.decisions.outcomes.forget(DecisionPath::Stall, member);
        self.start_decision(DecisionPath::Stall, member, &input, questions);
        if !self
            .decisions
            .jobs
            .contains_key(&(DecisionPath::Stall, key.clone()))
        {
            self.decisions.stall_deferred.remove(&key);
            return false;
        }
        let timeout = Duration::from_millis(
            self.config
                .team_config
                .workflow_policy
                .decisions
                .timeouts_ms
                .for_path(DecisionPath::Stall),
        );
        let since = match self.decisions.stall_deferred.peek(&key) {
            Some(since) => *since,
            None => {
                let now = Instant::now();
                self.decisions.stall_deferred.insert(key.clone(), now);
                now
            }
        };
        if since.elapsed() < timeout {
            return true;
        }
        self.decisions.stall_deferred.remove(&key);
        false
    }

    /// The cached Jev stall reason for `member`, only when Jev judged it
    /// stuck. Read-only.
    pub fn jev_stall_reason(&self, member: &str) -> Option<&str> {
        if !self.decision_path_configured(DecisionPath::Stall) {
            return None;
        }
        match self.cached_decision(DecisionPath::Stall, member) {
            Some(DecisionOutcome::Stall(StallOutcome {
                stuck: true,
                reason,
            })) => Some(reason.as_deref().unwrap_or("other")),
            _ => None,
        }
    }

    /// Test hook: store a stall outcome as if a call had answered.
    #[cfg(test)]
    pub(crate) fn seed_stall_outcome(&mut self, member: &str, outcome: StallOutcome) {
        self.decisions.outcomes.record(
            DecisionPath::Stall,
            member,
            0,
            DecisionOutcome::Stall(outcome),
        );
    }

    /// Test hook: store a completion outcome as if a call had answered.
    #[cfg(test)]
    pub(crate) fn seed_completion_outcome(&mut self, task_id: u32, outcome: CompletionOutcome) {
        self.decisions.outcomes.record(
            DecisionPath::Completion,
            &task_id.to_string(),
            0,
            DecisionOutcome::Completion(outcome),
        );
    }

    /// Test hook: number of decision calls in flight.
    #[cfg(test)]
    pub(crate) fn decision_jobs_in_flight(&self) -> usize {
        self.decisions.jobs.len()
    }

    /// The latest mapped outcome for `(path, key)`, for every path except
    /// triage. `key` is the member for screen state and stall, and the task id
    /// for completion and rubric. A miss means "use the heuristic".
    pub fn cached_decision(&self, path: DecisionPath, key: &str) -> Option<&DecisionOutcome> {
        self.decisions.outcomes.answer(path, key)
    }

    /// The triage verdict for a message body whose redacted text hashes to
    /// `body_hash` ([`gate::text_hash`] of the redacted body).
    pub fn cached_triage(&self, body_hash: u64) -> Option<TriageOutcome> {
        self.decisions.triage.peek(&body_hash).copied()
    }

    /// Start a decision job unless one is unnecessary: the path is disabled,
    /// the redacted input is unchanged since the last answer, a call for the
    /// same key is in flight, the input failed recently, or the daily budget
    /// is spent. Never blocks. For [`DecisionPath::Triage`] `key` is ignored
    /// and the hash of the redacted body identifies the job.
    pub fn start_decision(
        &mut self,
        path: DecisionPath,
        key: &str,
        raw_input: &str,
        questions: Vec<Question>,
    ) {
        self.spawn_decision(path, key, raw_input, questions, Vec::new());
    }

    /// Start a rubric decision for task `key`. The criteria are redacted
    /// before they become question text.
    pub fn start_rubric_decision(&mut self, key: &str, raw_input: &str, criteria: &[String]) {
        let criteria: Vec<String> = criteria.iter().map(|c| redact::redact(c)).collect();
        let questions = paths::rubric_questions(&criteria);
        self.spawn_decision(DecisionPath::Rubric, key, raw_input, questions, criteria);
    }

    fn spawn_decision(
        &mut self,
        path: DecisionPath,
        key: &str,
        raw_input: &str,
        questions: Vec<Question>,
        criteria: Vec<String>,
    ) {
        if !self.decisions_enabled(path) {
            return;
        }
        let input = redact::redact(raw_input);
        let gate_hash = gate::input_hash(&input, &questions);
        let triage_key = gate::text_hash(&input);
        let key = if path == DecisionPath::Triage {
            format!("{triage_key:016x}")
        } else {
            key.to_string()
        };
        let job_key = (path, key.clone());
        if self.decisions.jobs.contains_key(&job_key) {
            return;
        }
        let answered = if path == DecisionPath::Triage {
            self.decisions.triage.peek(&triage_key).is_some()
        } else {
            self.decisions.outcomes.unchanged(path, &key, gate_hash)
        };
        if answered {
            return;
        }
        if let Some((hash, when)) = self.decisions.failed.peek(&job_key) {
            if *hash == gate_hash && when.elapsed() < FAILURE_RETRY_COOLDOWN {
                return;
            }
        }
        if self.decision_over_budget(path) {
            return;
        }
        let Some(api_key) = self.decision_api_key() else {
            return;
        };

        let policy = &self.config.team_config.workflow_policy.decisions;
        let timeout = Duration::from_millis(policy.timeouts_ms.for_path(path));
        let endpoint = policy.endpoint.clone();
        let body = client::build_request(
            &policy.model,
            &input,
            &questions,
            policy.zero_data_retention,
        );
        let job = TestJob::spawn_with_timeout(
            format!("decide-{}-{key}", path.label()),
            Some(timeout),
            move |_pgid| {
                client::send(&endpoint, &api_key, &body, timeout).map_err(anyhow::Error::from)
            },
        );
        self.decisions.jobs.insert(
            job_key,
            DecisionJob {
                job,
                started: Instant::now(),
                gate_hash,
                triage_key,
                criteria,
            },
        );
    }

    /// Spend recorded today: the larger of the persisted total and this
    /// session's own tally.
    fn decision_spent_today(&self, date: &str) -> f64 {
        let persisted = self
            .telemetry_db
            .as_ref()
            .and_then(|conn| budget::spend_on(conn, date).ok())
            .unwrap_or(0.0);
        let session = self
            .decisions
            .session_spend
            .as_ref()
            .filter(|(day, _)| day == date)
            .map_or(0.0, |(_, usd)| *usd);
        persisted.max(session)
    }

    /// True when today's spend has reached the cap. The first hit each UTC day
    /// emits `decision_fallback` (budget) and `decision_budget_exceeded`.
    fn decision_over_budget(&mut self, path: DecisionPath) -> bool {
        let cap = self
            .config
            .team_config
            .workflow_policy
            .decisions
            .daily_spend_cap_usd;
        let date = budget::today_utc();
        let spent = self.decision_spent_today(&date);
        if spent < cap {
            return false;
        }
        if self.decisions.budget_reported_on.as_deref() != Some(date.as_str()) {
            self.decisions.budget_reported_on = Some(date.clone());
            self.emit_event(TeamEvent::decision_fallback(
                path.label(),
                None,
                &Fallback::Budget.to_string(),
            ));
            self.emit_event(TeamEvent::decision_budget_exceeded(&date, spent, cap));
        }
        true
    }

    /// Persist the cost of a finished call. Runs on the poll thread because
    /// the telemetry connection is not `Sync`.
    fn record_decision_spend(&mut self, cost_usd: f64) {
        let date = budget::today_utc();
        if let Some(conn) = &self.telemetry_db {
            if let Err(error) = budget::add_spend(conn, &date, cost_usd) {
                debug!(error = %error, "failed to persist decision spend; continuing");
            }
        }
        let session = self
            .decisions
            .session_spend
            .get_or_insert_with(|| (date.clone(), 0.0));
        if session.0 != date {
            *session = (date, 0.0);
        }
        session.1 += cost_usd;
    }

    /// Poll-loop step: take finished decision jobs, record spend, store the
    /// mapped outcome and emit `decision_made` or `decision_fallback`.
    pub(crate) fn poll_decisions(&mut self) -> Result<()> {
        if self.decisions.jobs.is_empty() {
            return Ok(());
        }
        let finished: Vec<(JobKey, anyhow::Result<DecisionResponse>)> = self
            .decisions
            .jobs
            .iter()
            .filter_map(|(key, job)| job.job.try_take().map(|result| (key.clone(), result)))
            .collect();
        for (job_key, result) in finished {
            if let Some(job) = self.decisions.jobs.remove(&job_key) {
                self.finish_decision(job_key, job, result);
            }
        }
        Ok(())
    }

    fn finish_decision(
        &mut self,
        job_key: JobKey,
        job: DecisionJob,
        result: anyhow::Result<DecisionResponse>,
    ) {
        let (path, key) = &job_key;
        let latency_ms = u64::try_from(job.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let min_confidence = self
            .config
            .team_config
            .workflow_policy
            .decisions
            .min_confidence;
        let mapped = match result {
            Ok(response) => {
                // A refusal or unusable answer is still billed.
                self.record_decision_spend(response.cost_usd);
                map_outcome(*path, &response.answers, &job.criteria, min_confidence)
                    .map(|outcome| (outcome, response.cost_usd))
            }
            Err(error) => Err(fallback_of(&error)),
        };
        match mapped {
            Ok((outcome, cost_usd)) => {
                self.decisions.failed.remove(&job_key);
                match outcome {
                    DecisionOutcome::Triage(verdict) => {
                        self.decisions.triage.insert(job.triage_key, verdict);
                    }
                    other => self
                        .decisions
                        .outcomes
                        .record(*path, key, job.gate_hash, other),
                }
                self.emit_event(TeamEvent::decision_made(
                    path.label(),
                    key,
                    "jev",
                    latency_ms,
                    false,
                    cost_usd,
                ));
                if *path == DecisionPath::Rubric {
                    self.deliver_rubric(key);
                }
                if *path == DecisionPath::ScreenState {
                    self.apply_screen_outcome(key);
                }
            }
            Err(fallback) => {
                // A stale outcome must not outlive a failed refresh.
                self.decisions.outcomes.forget(*path, key);
                self.decisions
                    .failed
                    .insert(job_key.clone(), (job.gate_hash, Instant::now()));
                self.emit_event(TeamEvent::decision_fallback(
                    path.label(),
                    Some(key),
                    &fallback.to_string(),
                ));
                if *path == DecisionPath::Rubric {
                    self.decisions.rubric_recipients.remove(key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::team::config::{DecisionTimeouts, DecisionsConfig, WorkflowPolicy};
    use crate::team::decide::paths::{ScreenOutcome, triage_questions};
    use crate::team::decide::test_server::serve_once;
    use crate::team::events::{TeamEvent, read_events};
    use crate::team::test_support::{EnvVarGuard, TestDaemonBuilder};

    const SECRET_KEY: &str = "gateway-key-for-tests-0123456789";

    fn daemon_with(
        root: &std::path::Path,
        endpoint: &str,
        key_env: &str,
        configure: impl FnOnce(&mut DecisionsConfig),
    ) -> TeamDaemon {
        let mut decisions = DecisionsConfig {
            provider: DecisionProvider::Jev,
            endpoint: endpoint.to_string(),
            api_key_env: key_env.to_string(),
            ..DecisionsConfig::default()
        };
        configure(&mut decisions);
        TestDaemonBuilder::new(root)
            .workflow_policy(WorkflowPolicy {
                decisions,
                ..WorkflowPolicy::default()
            })
            .build()
    }

    fn events(daemon: &TeamDaemon) -> Vec<TeamEvent> {
        read_events(&crate::team::team_events_path(&daemon.config.project_root)).unwrap()
    }

    fn named(events: &[TeamEvent], name: &str) -> Vec<TeamEvent> {
        events.iter().filter(|e| e.event == name).cloned().collect()
    }

    fn drain(daemon: &mut TeamDaemon) {
        for _ in 0..500 {
            daemon.poll_decisions().unwrap();
            if daemon.decisions.jobs.is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("decision jobs did not finish");
    }

    fn triage_reply(value: &str, confidence: f64, cost: &str) -> String {
        json!({
            "answers": [{"type": "choice", "name": "kind", "choice": value, "confidence": confidence}],
            "usage": {"input_tokens": 12},
            "provider_metadata": {"gateway": {"cost": cost}}
        })
        .to_string()
    }

    #[test]
    fn heuristic_provider_never_spawns() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        assert!(!daemon.decisions_enabled(DecisionPath::Triage));
        daemon.start_decision(DecisionPath::Triage, "m", "hello", triage_questions());
        assert!(daemon.decisions.jobs.is_empty());
        assert!(events(&daemon).is_empty());
    }

    #[test]
    fn unlisted_path_never_spawns() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_UNLISTED", SECRET_KEY);
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(
            tmp.path(),
            "http://127.0.0.1:1/v1/decisions",
            "BATTY_TEST_JEV_UNLISTED",
            |d| d.paths = vec![DecisionPath::Stall],
        );
        assert!(!daemon.decisions_enabled(DecisionPath::Triage));
        assert!(daemon.decisions_enabled(DecisionPath::Stall));
        daemon.start_decision(DecisionPath::Triage, "m", "hello", triage_questions());
        assert!(daemon.decisions.jobs.is_empty());
    }

    #[test]
    fn missing_key_reports_once_and_never_spawns() {
        let _key = EnvVarGuard::unset("BATTY_TEST_JEV_NO_KEY");
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(
            tmp.path(),
            "http://127.0.0.1:1/v1/decisions",
            "BATTY_TEST_JEV_NO_KEY",
            |_| {},
        );
        for _ in 0..3 {
            daemon.start_decision(DecisionPath::Triage, "m", "hello", triage_questions());
            daemon.poll_decisions().unwrap();
        }
        assert!(daemon.decisions.jobs.is_empty());
        let fallbacks = named(&events(&daemon), "decision_fallback");
        assert_eq!(fallbacks.len(), 1);
        assert_eq!(fallbacks[0].reason.as_deref(), Some("no_key"));
    }

    #[test]
    fn exhausted_budget_never_spawns_and_reports_once_per_day() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_BUDGET", SECRET_KEY);
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(
            tmp.path(),
            "http://127.0.0.1:1/v1/decisions",
            "BATTY_TEST_JEV_BUDGET",
            |d| d.daily_spend_cap_usd = 1.0,
        );
        budget::add_spend(
            daemon.telemetry_db.as_ref().unwrap(),
            &budget::today_utc(),
            1.0,
        )
        .unwrap();
        daemon.start_decision(DecisionPath::Triage, "m", "first", triage_questions());
        daemon.start_decision(DecisionPath::Triage, "m", "second", triage_questions());
        assert!(daemon.decisions.jobs.is_empty());
        let all = events(&daemon);
        let fallbacks = named(&all, "decision_fallback");
        assert_eq!(fallbacks.len(), 1);
        assert_eq!(fallbacks[0].reason.as_deref(), Some("budget"));
        let exceeded = named(&all, "decision_budget_exceeded");
        assert_eq!(exceeded.len(), 1);
        assert_eq!(
            exceeded[0].reason.as_deref(),
            Some(budget::today_utc().as_str())
        );
    }

    #[test]
    fn session_spend_blocks_when_the_database_is_missing() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_SESSION", SECRET_KEY);
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(
            tmp.path(),
            "http://127.0.0.1:1/v1/decisions",
            "BATTY_TEST_JEV_SESSION",
            |d| d.daily_spend_cap_usd = 0.5,
        );
        daemon.telemetry_db = None;
        daemon.record_decision_spend(0.6);
        daemon.start_decision(DecisionPath::Triage, "m", "hello", triage_questions());
        assert!(daemon.decisions.jobs.is_empty());
    }

    #[test]
    fn finished_job_records_spend_stores_outcome_and_emits_decision_made() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_OK", SECRET_KEY);
        let (endpoint, rx, server) = serve_once(
            "HTTP/1.1 200 OK",
            triage_reply("noise", 0.9, "0.0001"),
            Duration::ZERO,
        );
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(tmp.path(), &endpoint, "BATTY_TEST_JEV_OK", |_| {});
        daemon.start_decision(
            DecisionPath::Triage,
            "ignored",
            "hello there",
            triage_questions(),
        );
        assert_eq!(daemon.decisions.jobs.len(), 1);
        drain(&mut daemon);
        server.join().unwrap();

        let body_hash = gate::text_hash(&redact::redact("hello there"));
        assert_eq!(daemon.cached_triage(body_hash), Some(TriageOutcome::Noise));
        let conn = daemon.telemetry_db.as_ref().unwrap();
        let today = budget::today_utc();
        assert!((budget::spend_on(conn, &today).unwrap() - 0.0001).abs() < 1e-12);
        assert_eq!(budget::calls_on(conn, &today).unwrap(), 1);

        let made = named(&events(&daemon), "decision_made");
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].decision_path.as_deref(), Some("triage"));
        assert_eq!(made[0].provider.as_deref(), Some("jev"));
        assert_eq!(made[0].cached, Some(false));
        assert_eq!(made[0].cost_usd, Some(0.0001));

        let request = rx.recv().unwrap().to_ascii_lowercase();
        assert!(request.contains(&format!("authorization: bearer {SECRET_KEY}")));
    }

    #[test]
    fn unchanged_input_hits_the_gate_and_spawns_nothing() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_GATE", SECRET_KEY);
        let reply = json!({
            "answers": [{"type": "choice", "name": "state", "choice": "idle", "confidence": 0.95}],
            "provider_metadata": {"gateway": {"cost": "0.00001"}}
        })
        .to_string();
        let (endpoint, _rx, server) = serve_once("HTTP/1.1 200 OK", reply, Duration::ZERO);
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(tmp.path(), &endpoint, "BATTY_TEST_JEV_GATE", |_| {});
        let questions = paths::screen_questions;
        daemon.start_decision(
            DecisionPath::ScreenState,
            "manager",
            "$ prompt",
            questions(),
        );
        drain(&mut daemon);
        server.join().unwrap();
        assert_eq!(
            daemon.cached_decision(DecisionPath::ScreenState, "manager"),
            Some(&DecisionOutcome::Screen(ScreenOutcome::Idle))
        );

        // The one-shot server is gone: any new call would fail loudly.
        daemon.start_decision(
            DecisionPath::ScreenState,
            "manager",
            "$ prompt",
            questions(),
        );
        assert!(daemon.decisions.jobs.is_empty());
        // New input does start a job.
        daemon.start_decision(DecisionPath::ScreenState, "manager", "$ other", questions());
        assert_eq!(daemon.decisions.jobs.len(), 1);
        // And an in-flight key is not duplicated.
        daemon.start_decision(DecisionPath::ScreenState, "manager", "$ third", questions());
        assert_eq!(daemon.decisions.jobs.len(), 1);
        drain(&mut daemon);
    }

    #[test]
    fn http_error_falls_back_and_is_not_retried_immediately() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_HTTP", SECRET_KEY);
        let (endpoint, _rx, server) = serve_once(
            "HTTP/1.1 503 Service Unavailable",
            r#"{"error":{"message":"boom"}}"#.into(),
            Duration::ZERO,
        );
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(tmp.path(), &endpoint, "BATTY_TEST_JEV_HTTP", |_| {});
        daemon.start_decision(DecisionPath::Triage, "m", "hello", triage_questions());
        drain(&mut daemon);
        server.join().unwrap();

        let fallbacks = named(&events(&daemon), "decision_fallback");
        assert_eq!(fallbacks.len(), 1);
        assert_eq!(fallbacks[0].reason.as_deref(), Some("http_503"));
        assert_eq!(fallbacks[0].decision_path.as_deref(), Some("triage"));
        let body_hash = gate::text_hash("hello");
        assert_eq!(daemon.cached_triage(body_hash), None);
        // Failed input is on cooldown.
        daemon.start_decision(DecisionPath::Triage, "m", "hello", triage_questions());
        assert!(daemon.decisions.jobs.is_empty());
        // No spend for a failed call.
        let conn = daemon.telemetry_db.as_ref().unwrap();
        assert_eq!(budget::calls_on(conn, &budget::today_utc()).unwrap(), 0);
    }

    #[test]
    fn low_confidence_falls_back_but_is_still_billed() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_LOW", SECRET_KEY);
        let (endpoint, _rx, server) = serve_once(
            "HTTP/1.1 200 OK",
            triage_reply("noise", 0.2, "0.0002"),
            Duration::ZERO,
        );
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(tmp.path(), &endpoint, "BATTY_TEST_JEV_LOW", |_| {});
        daemon.start_decision(DecisionPath::Triage, "m", "hello", triage_questions());
        drain(&mut daemon);
        server.join().unwrap();

        let fallbacks = named(&events(&daemon), "decision_fallback");
        assert_eq!(fallbacks[0].reason.as_deref(), Some("low_confidence"));
        assert!(named(&events(&daemon), "decision_made").is_empty());
        let conn = daemon.telemetry_db.as_ref().unwrap();
        assert!((budget::spend_on(conn, &budget::today_utc()).unwrap() - 0.0002).abs() < 1e-12);
    }

    #[test]
    fn slow_server_times_out_and_falls_back() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_SLOW", SECRET_KEY);
        let (endpoint, _rx, server) = serve_once(
            "HTTP/1.1 200 OK",
            triage_reply("noise", 0.9, "0.0001"),
            Duration::from_millis(1500),
        );
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(tmp.path(), &endpoint, "BATTY_TEST_JEV_SLOW", |d| {
            d.timeouts_ms = DecisionTimeouts {
                triage: 200,
                ..DecisionTimeouts::default()
            }
        });
        daemon.start_decision(DecisionPath::Triage, "m", "hello", triage_questions());
        drain(&mut daemon);
        server.join().unwrap();
        let fallbacks = named(&events(&daemon), "decision_fallback");
        assert_eq!(fallbacks.len(), 1);
        assert_eq!(fallbacks[0].reason.as_deref(), Some("timeout"));
    }

    #[test]
    fn failed_refresh_drops_the_stale_outcome() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_STALE", SECRET_KEY);
        let ok = json!({
            "answers": [{"type": "choice", "name": "state", "choice": "working", "confidence": 0.9}]
        })
        .to_string();
        let (first, _rx1, s1) = serve_once("HTTP/1.1 200 OK", ok, Duration::ZERO);
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(tmp.path(), &first, "BATTY_TEST_JEV_STALE", |_| {});
        daemon.start_decision(
            DecisionPath::ScreenState,
            "manager",
            "one",
            paths::screen_questions(),
        );
        drain(&mut daemon);
        s1.join().unwrap();
        assert!(
            daemon
                .cached_decision(DecisionPath::ScreenState, "manager")
                .is_some()
        );

        let (second, _rx2, s2) = serve_once(
            "HTTP/1.1 500 Internal Server Error",
            "{}".into(),
            Duration::ZERO,
        );
        daemon.config.team_config.workflow_policy.decisions.endpoint = second;
        daemon.start_decision(
            DecisionPath::ScreenState,
            "manager",
            "two",
            paths::screen_questions(),
        );
        drain(&mut daemon);
        s2.join().unwrap();
        assert!(
            daemon
                .cached_decision(DecisionPath::ScreenState, "manager")
                .is_none()
        );
    }

    #[test]
    fn secrets_never_reach_the_request_body_or_the_event_log() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_REDACT", SECRET_KEY);
        let planted = format!("{}{}", "sk-", "ant-api03-PlantedSecretValue0123456789");
        let (endpoint, rx, server) = serve_once(
            "HTTP/1.1 200 OK",
            triage_reply("actionable", 0.9, "0.0001"),
            Duration::ZERO,
        );
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(tmp.path(), &endpoint, "BATTY_TEST_JEV_REDACT", |_| {});
        daemon.start_decision(
            DecisionPath::Triage,
            "m",
            &format!("please use {planted} for the deploy"),
            triage_questions(),
        );
        drain(&mut daemon);
        server.join().unwrap();

        let request = rx.recv().unwrap();
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        assert!(!body.contains("PlantedSecretValue"));
        assert!(body.contains("[REDACTED:anthropic_key]"));
        assert!(!body.contains(SECRET_KEY));
        let log =
            std::fs::read_to_string(crate::team::team_events_path(&daemon.config.project_root))
                .unwrap();
        assert!(!log.contains("PlantedSecretValue"));
        assert!(!log.contains(SECRET_KEY));
    }

    #[test]
    fn poll_decisions_with_no_jobs_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.poll_decisions().unwrap();
        assert!(daemon.decisions.jobs.is_empty());
    }

    #[test]
    fn fallback_of_classifies_job_errors() {
        let direct = anyhow::Error::from(Fallback::Http(429));
        assert_eq!(fallback_of(&direct), Fallback::Http(429));
        let timed_out = anyhow::Error::new(TestTimedOut { secs: 2 });
        assert_eq!(fallback_of(&timed_out), Fallback::Timeout);
        assert_eq!(
            fallback_of(&anyhow::anyhow!("worker died")),
            Fallback::Http(0)
        );
    }

    #[test]
    fn rubric_decision_maps_criteria_scores() {
        let _key = EnvVarGuard::set("BATTY_TEST_JEV_RUBRIC", SECRET_KEY);
        let reply = json!({
            "answers": [
                {"type": "score", "name": "criterion_1", "score": 1.8, "confidence": 0.9},
                {"type": "predicate", "name": "criteria_met", "probability": 0.95}
            ],
            "provider_metadata": {"gateway": {"cost": "0.0003"}}
        })
        .to_string();
        let (endpoint, rx, server) = serve_once("HTTP/1.1 200 OK", reply, Duration::ZERO);
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = daemon_with(tmp.path(), &endpoint, "BATTY_TEST_JEV_RUBRIC", |_| {});
        daemon.start_rubric_decision("7", "task and diff", &["adds a flag".to_string()]);
        drain(&mut daemon);
        server.join().unwrap();
        match daemon.cached_decision(DecisionPath::Rubric, "7") {
            Some(DecisionOutcome::Rubric(outcome)) => {
                assert!(outcome.criteria_met);
                assert_eq!(outcome.scores, vec![("adds a flag".to_string(), 1.8)]);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert!(rx.recv().unwrap().contains("adds a flag"));
    }
}
