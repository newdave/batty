use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
#[cfg(test)]
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::TeamDaemon;
use crate::task::load_tasks_from_dir;
use crate::team::board::{WorkflowMetadata, read_workflow_metadata};
use crate::team::daemon::verification::VerificationRunResult;
use crate::team::merge::{
    DeferredDirectMerge, MergeLock, MergeMode, MergeOutcome, RootCheckoutDirty, RootDirtyState,
    infer_merge_mode_from_failure, inspect_root_dirty_state, merge_engineer_branch_into_trunk,
};
use crate::team::task_loop::{
    current_worktree_branch, read_task_title, run_tests_in_worktree_tracked,
};
use crate::team::test_job::TestJob;

/// A request whose merge attempt fails before anything lands on trunk (lock
/// I/O, git inspection, board reads, ...) is retried with backoff and
/// escalated after this many attempts.
pub(crate) const MAX_MERGE_ATTEMPTS: u32 = 5;
const MERGE_RETRY_BASE_DELAY: Duration = Duration::from_secs(5);
const MERGE_RETRY_MAX_DELAY: Duration = Duration::from_secs(300);
/// Wait this long before retrying when another holder has the merge lock.
const MERGE_LOCK_BUSY_DELAY: Duration = Duration::from_secs(2);
/// Consecutive errors taking the merge lock for an in-flight merge before
/// escalating; after that the lock is retried every
/// [`MERGE_LOCK_FAILURE_RETRY`] instead of every tick.
pub(crate) const MERGE_LOCK_FAILURE_ESCALATE_AFTER: u32 = 5;
const MERGE_LOCK_FAILURE_RETRY: Duration = Duration::from_secs(300);
/// A merge deferred because the root checkout's local changes are in the
/// way escalates after this many deferrals, or after
/// [`DIRTY_ROOT_ESCALATE_AFTER_AGE`], whichever comes first.
pub(crate) const DIRTY_ROOT_ESCALATE_AFTER_DEFERRALS: u32 = 3;
pub(crate) const DIRTY_ROOT_ESCALATE_AFTER_AGE: Duration = Duration::from_secs(30 * 60);
/// Checkout used for post-merge verification when the root checkout is not
/// on trunk (relative to the project root).
const VERIFY_WORKTREE_DIR: &str = ".batty/verify-worktree";

fn merge_retry_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    MERGE_RETRY_BASE_DELAY
        .saturating_mul(1u32 << shift)
        .min(MERGE_RETRY_MAX_DELAY)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MergeRequest {
    pub task_id: u32,
    pub engineer: String,
    pub branch: String,
    pub worktree_dir: PathBuf,
    #[serde(skip, default = "Instant::now")]
    pub queued_at: Instant,
    pub test_passed: bool,
    pub should_post_merge_verify: bool,
    pub test_duration_ms: u64,
    pub confidence: f64,
    pub files_changed: usize,
    pub lines_changed: usize,
    /// Which path queued the request.
    #[serde(default)]
    pub origin: MergeOrigin,
}

/// The path that queued a merge request. It selects the merge telemetry and
/// how the board is updated once the merge lands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MergeOrigin {
    /// Auto-merge policy accepted the completion.
    #[default]
    AutoMerge,
    /// Auto-merge is disabled: a verified completion merges without review.
    DirectCompletion,
    /// A chat (Telegram/Discord) `merge` command approved a review task.
    Chat,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeQueueOutcome {
    Success,
    Conflict,
    Reverted,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeQueueEvent {
    pub task_id: u32,
    pub engineer: String,
    pub outcome: MergeQueueOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeQueueLastResult {
    task_id: u32,
    outcome: MergeQueueOutcome,
    finished_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoMergeSkipReason {
    WrongStatus,
    MissingPacket,
    NoBranch,
}

impl AutoMergeSkipReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::WrongStatus => "wrong_status",
            Self::MissingPacket => "missing_packet",
            Self::NoBranch => "no_branch",
        }
    }
}

/// State of the post-merge verification of a merge that already landed.
pub(crate) enum PostMergeVerify {
    /// The test command is running on a background thread.
    Running(TestJob<VerificationRunResult>),
    /// Interrupted by a restart or hot reload; starts again on the next poll.
    Pending,
    /// Verification failed (or could not run); the merge still has to be
    /// reverted once the merge lock is free. Holds the failure output.
    Failed(String),
}

impl std::fmt::Debug for PostMergeVerify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running(job) => write!(f, "Running({}s)", job.elapsed().as_secs()),
            Self::Pending => write!(f, "Pending"),
            Self::Failed(output) => write!(f, "Failed({} bytes)", output.len()),
        }
    }
}

/// A merge that landed on trunk and is waiting for post-merge verification.
/// The queue stays blocked until it is finalized or reverted.
#[derive(Debug)]
pub(crate) struct InFlightMerge {
    pub request: MergeRequest,
    /// `refs/heads/<trunk>` before the merge.
    pub pre_merge_head: String,
    /// `refs/heads/<trunk>` right after the merge. A revert only moves trunk
    /// when it still points here, so later commits are never discarded.
    pub post_merge_head: Option<String>,
    pub mode: MergeMode,
    pub merge_reason: Option<String>,
    pub verify: PostMergeVerify,
    /// The merge lock, held from the merge until the merge is finalized or
    /// reverted so nothing else (another process included) lands on trunk
    /// meanwhile. `None` after a restore until it is re-acquired.
    pub lock: Option<MergeLock>,
    /// The revert is waiting for the root checkout to be cleaned and that
    /// has been escalated already.
    pub dirty_revert_escalated: bool,
}

impl InFlightMerge {
    fn new(
        request: MergeRequest,
        pre_merge_head: String,
        post_merge_head: Option<String>,
        mode: MergeMode,
        merge_reason: Option<String>,
        verify: PostMergeVerify,
        lock: Option<MergeLock>,
    ) -> Self {
        Self {
            request,
            pre_merge_head,
            post_merge_head,
            mode,
            merge_reason,
            verify,
            lock,
            dirty_revert_escalated: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PersistedInFlightMerge {
    pub request: MergeRequest,
    pub pre_merge_head: String,
    #[serde(default)]
    pub post_merge_head: Option<String>,
    pub mode: MergeMode,
    #[serde(default)]
    pub merge_reason: Option<String>,
    /// Set when verification already failed and only the revert is pending.
    #[serde(default)]
    pub failure_output: Option<String>,
}

/// Merge-queue state that survives daemon restarts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PersistedMergeQueue {
    #[serde(default)]
    pub queue: Vec<MergeRequest>,
    #[serde(default)]
    pub in_flight: Option<PersistedInFlightMerge>,
    /// Failed pre-merge attempts per task id.
    #[serde(default)]
    pub attempts: HashMap<u32, u32>,
}

/// Result of one merge attempt, as reported by the processor.
#[derive(Debug)]
pub(crate) enum MergeAttempt {
    Done(MergeQueueOutcome),
    /// Another holder has the merge lock; retry on a later tick.
    LockBusy,
    /// The merge landed and post-merge verification is running.
    Verifying(Box<InFlightMerge>),
    /// Nothing landed: the root checkout's local changes are in the way.
    /// Retried on the normal backoff, escalated once it persists.
    DirtyRootDeferred(RootCheckoutDirty),
}

/// What the queue did with a finished attempt.
#[derive(Debug)]
pub(crate) enum MergeQueueStep {
    Finished(MergeQueueEvent),
    Deferred,
    VerificationStarted(u32),
    RetryScheduled {
        request: MergeRequest,
        attempt: u32,
        delay: Duration,
        error: anyhow::Error,
    },
    GaveUp {
        request: MergeRequest,
        attempts: u32,
        error: anyhow::Error,
    },
    DirtyRootDeferred {
        request: MergeRequest,
        deferrals: u32,
        delay: Duration,
        dirty: RootCheckoutDirty,
        /// First time this request crossed the escalation threshold.
        escalate: bool,
    },
}

/// Deferrals of one request because the root checkout was dirty.
#[derive(Debug, Clone, Copy)]
struct DirtyRootDeferrals {
    count: u32,
    first_at: Instant,
    escalated: bool,
}

#[derive(Debug, Default)]
pub(crate) struct MergeQueue {
    queue: VecDeque<MergeRequest>,
    /// The request being processed during this tick. Still set on the next
    /// tick only if processing panicked; it is then put back on the queue.
    active: Option<MergeRequest>,
    in_flight: Option<InFlightMerge>,
    attempts: HashMap<u32, u32>,
    /// Backoff for the request at the head of the queue.
    not_before: Option<Instant>,
    last_result: Option<MergeQueueLastResult>,
    last_reported_status: Option<String>,
    /// Dirty-root deferrals per task id (not persisted: the count restarts
    /// after a restart, which only delays the escalation).
    dirty_root_deferrals: HashMap<u32, DirtyRootDeferrals>,
    /// Consecutive errors taking the merge lock for the in-flight merge.
    lock_failures: u32,
    /// After repeated lock errors, do not retry before this instant.
    lock_retry_at: Option<Instant>,
}

impl MergeQueue {
    pub(crate) fn enqueue(&mut self, request: MergeRequest) {
        self.queue.push_back(request);
    }

    pub(crate) fn queued_len(&self) -> usize {
        self.queue.len()
    }

    #[cfg(test)]
    pub(crate) fn active_task_id(&self) -> Option<u32> {
        self.active.as_ref().map(|request| request.task_id)
    }

    /// True if `task_id` is queued, being merged, or being verified.
    pub(crate) fn contains_task(&self, task_id: u32) -> bool {
        self.queue
            .iter()
            .chain(self.active.iter())
            .chain(self.in_flight.iter().map(|merge| &merge.request))
            .any(|request| request.task_id == task_id)
    }

    /// True while a landed merge is waiting for post-merge verification.
    pub(crate) fn is_verifying(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Test helper: pretend `request` landed and awaits post-merge
    /// verification.
    #[cfg(test)]
    pub(crate) fn mark_in_flight_for_test(&mut self, request: MergeRequest) {
        self.in_flight = Some(InFlightMerge::new(
            request,
            String::new(),
            None,
            MergeMode::DirectRoot,
            None,
            PostMergeVerify::Pending,
            None,
        ));
    }

    #[cfg(test)]
    pub(crate) fn in_flight_task_id(&self) -> Option<u32> {
        self.in_flight.as_ref().map(|merge| merge.request.task_id)
    }

    /// If the previous attempt never reported back (it panicked), treat it as
    /// a failed attempt so the request is retried instead of lost.
    pub(crate) fn recover_interrupted_attempt(&mut self, now: Instant) -> Option<MergeQueueStep> {
        let request = self.active.take()?;
        Some(self.finish(
            request,
            Err(anyhow!(
                "merge attempt was interrupted (panic) before it finished"
            )),
            now,
        ))
    }

    /// Take the next request to merge, if the queue is not blocked by an
    /// in-flight merge or a retry backoff. The request stays recorded as
    /// active until [`MergeQueue::finish`].
    pub(crate) fn begin_next(&mut self, now: Instant) -> Option<MergeRequest> {
        if self.active.is_some() || self.in_flight.is_some() {
            return None;
        }
        if self.not_before.is_some_and(|not_before| now < not_before) {
            return None;
        }
        let request = self.queue.pop_front()?;
        self.not_before = None;
        self.active = Some(request.clone());
        Some(request)
    }

    pub(crate) fn finish(
        &mut self,
        request: MergeRequest,
        result: Result<MergeAttempt>,
        now: Instant,
    ) -> MergeQueueStep {
        self.active = None;
        if !matches!(
            result,
            Ok(MergeAttempt::DirtyRootDeferred(_) | MergeAttempt::LockBusy)
        ) {
            self.dirty_root_deferrals.remove(&request.task_id);
        }
        match result {
            Ok(MergeAttempt::DirtyRootDeferred(dirty)) => {
                let record = self.dirty_root_deferrals.entry(request.task_id).or_insert(
                    DirtyRootDeferrals {
                        count: 0,
                        first_at: now,
                        escalated: false,
                    },
                );
                record.count += 1;
                let due = record.count >= DIRTY_ROOT_ESCALATE_AFTER_DEFERRALS
                    || now.saturating_duration_since(record.first_at)
                        >= DIRTY_ROOT_ESCALATE_AFTER_AGE;
                let escalate = due && !record.escalated;
                record.escalated |= escalate;
                let deferrals = record.count;
                let delay = merge_retry_delay(deferrals);
                self.queue.push_front(request.clone());
                self.not_before = Some(now + delay);
                MergeQueueStep::DirtyRootDeferred {
                    request,
                    deferrals,
                    delay,
                    dirty,
                    escalate,
                }
            }
            Ok(MergeAttempt::Done(outcome)) => {
                self.attempts.remove(&request.task_id);
                self.record_result(request.task_id, outcome.clone());
                MergeQueueStep::Finished(MergeQueueEvent {
                    task_id: request.task_id,
                    engineer: request.engineer,
                    outcome,
                })
            }
            Ok(MergeAttempt::LockBusy) => {
                self.queue.push_front(request);
                self.not_before = Some(now + MERGE_LOCK_BUSY_DELAY);
                MergeQueueStep::Deferred
            }
            Ok(MergeAttempt::Verifying(in_flight)) => {
                self.attempts.remove(&request.task_id);
                let task_id = request.task_id;
                self.in_flight = Some(*in_flight);
                MergeQueueStep::VerificationStarted(task_id)
            }
            Err(error) => {
                let attempt = {
                    let count = self.attempts.entry(request.task_id).or_insert(0);
                    *count += 1;
                    *count
                };
                if attempt >= MAX_MERGE_ATTEMPTS {
                    self.attempts.remove(&request.task_id);
                    self.record_result(request.task_id, MergeQueueOutcome::Failed);
                    return MergeQueueStep::GaveUp {
                        request,
                        attempts: attempt,
                        error,
                    };
                }
                let delay = merge_retry_delay(attempt);
                self.queue.push_front(request.clone());
                self.not_before = Some(now + delay);
                MergeQueueStep::RetryScheduled {
                    request,
                    attempt,
                    delay,
                    error,
                }
            }
        }
    }

    fn take_in_flight(&mut self) -> Option<InFlightMerge> {
        self.in_flight.take()
    }

    fn record_result(&mut self, task_id: u32, outcome: MergeQueueOutcome) {
        self.last_result = Some(MergeQueueLastResult {
            task_id,
            outcome,
            finished_at: Instant::now(),
        });
    }

    /// Kill a running post-merge verification (shutdown, hot reload) and
    /// mark it to re-run after restart. Returns the task id if one was
    /// in flight.
    pub(crate) fn suspend_in_flight(&mut self) -> Option<u32> {
        let in_flight = self.in_flight.as_mut()?;
        if let PostMergeVerify::Running(job) = &in_flight.verify {
            job.kill();
            in_flight.verify = PostMergeVerify::Pending;
        }
        Some(in_flight.request.task_id)
    }

    pub(crate) fn snapshot(&self) -> PersistedMergeQueue {
        PersistedMergeQueue {
            queue: self
                .active
                .iter()
                .chain(self.queue.iter())
                .cloned()
                .collect(),
            in_flight: self.in_flight.as_ref().map(|merge| PersistedInFlightMerge {
                request: merge.request.clone(),
                pre_merge_head: merge.pre_merge_head.clone(),
                post_merge_head: merge.post_merge_head.clone(),
                mode: merge.mode,
                merge_reason: merge.merge_reason.clone(),
                failure_output: match &merge.verify {
                    PostMergeVerify::Failed(output) => Some(output.clone()),
                    _ => None,
                },
            }),
            attempts: self.attempts.clone(),
        }
    }

    /// Restore persisted state. Requests already queued in memory are kept
    /// behind the restored ones. A restored in-flight merge re-runs its
    /// verification (or its pending revert) on the next poll.
    pub(crate) fn restore(&mut self, persisted: PersistedMergeQueue) {
        let mut queue: VecDeque<MergeRequest> = persisted.queue.into();
        for request in self.queue.drain(..) {
            if !queue.iter().any(|queued| queued.task_id == request.task_id) {
                queue.push_back(request);
            }
        }
        self.queue = queue;
        for (task_id, attempts) in persisted.attempts {
            self.attempts.entry(task_id).or_insert(attempts);
        }
        if self.in_flight.is_none() {
            self.in_flight = persisted.in_flight.map(|merge| {
                InFlightMerge::new(
                    merge.request,
                    merge.pre_merge_head,
                    merge.post_merge_head,
                    merge.mode,
                    merge.merge_reason,
                    match merge.failure_output {
                        Some(output) => PostMergeVerify::Failed(output),
                        None => PostMergeVerify::Pending,
                    },
                    // Re-acquired by the daemon before the merge advances.
                    None,
                )
            });
        }
    }

    /// Test convenience: run one full attempt through `processor`.
    #[cfg(test)]
    pub(crate) fn process_next<F>(&mut self, processor: F) -> Option<MergeQueueStep>
    where
        F: FnOnce(&MergeRequest) -> Result<MergeAttempt>,
    {
        let now = Instant::now();
        if let Some(step) = self.recover_interrupted_attempt(now) {
            return Some(step);
        }
        let request = self.begin_next(now)?;
        let result = processor(&request);
        Some(self.finish(request, result, now))
    }

    fn status_line(&self) -> Option<String> {
        if self.queue.is_empty()
            && self.active.is_none()
            && self.in_flight.is_none()
            && self.last_result.is_none()
        {
            return None;
        }

        let queued = self.queue.len();
        let merging = if let Some(merge) = self.in_flight.as_ref() {
            format!(
                "#{} ({}) verifying",
                merge.request.task_id, merge.request.branch
            )
        } else {
            self.active
                .as_ref()
                .map(|request| format!("#{} ({})", request.task_id, request.branch))
                .unwrap_or_else(|| "idle".to_string())
        };
        let last = self
            .last_result
            .as_ref()
            .map(|result| {
                format!(
                    "#{} {} {}s ago",
                    result.task_id,
                    match result.outcome {
                        MergeQueueOutcome::Success => "merged",
                        MergeQueueOutcome::Conflict => "conflicted",
                        MergeQueueOutcome::Reverted => "reverted",
                        MergeQueueOutcome::Skipped => "skipped",
                        MergeQueueOutcome::Failed => "failed",
                    },
                    result.finished_at.elapsed().as_secs()
                )
            })
            .unwrap_or_else(|| "none".to_string());

        Some(format!(
            "[merge] queued: {queued} | merging: {merging} | last: {last}"
        ))
    }

    pub(crate) fn take_status_update(&mut self) -> Option<String> {
        let status = self.status_line()?;
        if self.last_reported_status.as_deref() == Some(status.as_str()) {
            return None;
        }
        self.last_reported_status = Some(status.clone());
        Some(status)
    }
}

/// How to undo a merge whose post-merge verification failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RevertPlan {
    /// The trunk ref still points at the merge result: move it back to the
    /// pre-merge head.
    Reset,
    /// Trunk is already back at the pre-merge head.
    AlreadyReverted,
    /// Trunk cannot be reverted automatically without discarding other work.
    Manual(String),
}

fn plan_post_merge_revert(
    current_head: Result<String>,
    pre_merge_head: &str,
    post_merge_head: Option<&str>,
) -> RevertPlan {
    let current = match current_head {
        Ok(head) => head,
        Err(error) => {
            return RevertPlan::Manual(format!("could not read the trunk ref ({error:#})"));
        }
    };
    if current == pre_merge_head {
        return RevertPlan::AlreadyReverted;
    }
    match post_merge_head {
        Some(post) if post == current => RevertPlan::Reset,
        Some(post) => RevertPlan::Manual(format!(
            "trunk moved from the merge result {post} to {current} during verification"
        )),
        None => RevertPlan::Manual("the merge result commit was not recorded".to_string()),
    }
}

impl TeamDaemon {
    pub(super) fn process_merge_queue(&mut self) -> Result<()> {
        let before = self.merge.queue.snapshot();
        self.process_merge_queue_step(Instant::now());
        if self.merge.queue.snapshot() != before {
            self.persist_merge_queue_change();
        }
        if let Some(status) = self.merge.queue.take_status_update() {
            self.record_orchestrator_action(status);
        }
        Ok(())
    }

    fn process_merge_queue_step(&mut self, now: Instant) {
        if let Some(step) = self.merge.queue.recover_interrupted_attempt(now) {
            self.handle_merge_queue_step(step);
        }
        self.advance_in_flight_merge();
        if self.merge.queue.is_verifying() {
            return;
        }
        // The main smoke check runs on the root checkout; merging under it
        // would change the files it is testing.
        if self.verification.background_tests.main_smoke.is_some() {
            return;
        }
        let Some(request) = self.merge.queue.begin_next(now) else {
            return;
        };
        debug!(
            task_id = request.task_id,
            queued = self.merge.queue.queued_len(),
            "processing merge queue"
        );
        let result = self.execute_queued_merge(&request);
        let step = self.merge.queue.finish(request, result, now);
        self.handle_merge_queue_step(step);
    }

    fn handle_merge_queue_step(&mut self, step: MergeQueueStep) {
        match step {
            MergeQueueStep::Finished(event) => {
                info!(
                    task_id = event.task_id,
                    engineer = %event.engineer,
                    outcome = ?event.outcome,
                    "merge queue processed request"
                );
            }
            MergeQueueStep::Deferred => {
                debug!("merge lock busy; merge queue will retry on a later tick");
            }
            MergeQueueStep::VerificationStarted(task_id) => {
                self.record_orchestrator_action(format!(
                    "merge queue: task #{task_id} merged; post-merge verification running in the background"
                ));
            }
            MergeQueueStep::RetryScheduled {
                request,
                attempt,
                delay,
                error,
            } => {
                warn!(
                    task_id = request.task_id,
                    engineer = %request.engineer,
                    attempt,
                    error = %format!("{error:#}"),
                    "merge attempt failed before merging; will retry"
                );
                self.record_orchestrator_action(format!(
                    "merge queue: task #{} attempt {attempt}/{MAX_MERGE_ATTEMPTS} failed before merging ({error:#}); retrying in {}s",
                    request.task_id,
                    delay.as_secs()
                ));
            }
            MergeQueueStep::DirtyRootDeferred {
                request,
                deferrals,
                delay,
                dirty,
                escalate,
            } => self.handle_dirty_root_deferral(&request, deferrals, delay, &dirty, escalate),
            MergeQueueStep::GaveUp {
                request,
                attempts,
                error,
            } => {
                warn!(
                    task_id = request.task_id,
                    engineer = %request.engineer,
                    attempts,
                    error = %format!("{error:#}"),
                    "merge queue gave up on request"
                );
                self.record_orchestrator_action(format!(
                    "merge queue: gave up on task #{} after {attempts} failed attempts ({error:#})",
                    request.task_id
                ));
                let notice = format!(
                    "Task #{} from {} could not be auto-merged after {attempts} attempts.\nLast error: {error:#}\nThe task stays in review; merge it manually or re-queue it once the cause is fixed.",
                    request.task_id, request.engineer
                );
                if let Some(manager_name) = self.manager_name(&request.engineer) {
                    self.notify_best_effort("daemon", &manager_name, &notice);
                    self.mark_member_working(&manager_name);
                    self.notify_reports_to_best_effort(&manager_name, &notice);
                }
                self.record_task_escalated(
                    &request.engineer,
                    request.task_id.to_string(),
                    Some("merge_queue_retries_exhausted"),
                );
            }
        }
    }

    fn handle_dirty_root_deferral(
        &mut self,
        request: &MergeRequest,
        deferrals: u32,
        delay: Duration,
        dirty: &RootCheckoutDirty,
        escalate: bool,
    ) {
        let paths = if dirty.paths.is_empty() {
            "(git did not list them)".to_string()
        } else {
            dirty.paths.join(", ")
        };
        warn!(
            task_id = request.task_id,
            engineer = %request.engineer,
            deferrals,
            paths = %paths,
            "merge deferred: root checkout has local changes in the way"
        );
        self.record_orchestrator_action(format!(
            "merge queue: task #{} deferred ({deferrals}x) because the root {} checkout has local changes in the way ({paths}); retrying in {}s",
            request.task_id,
            dirty.trunk_branch,
            delay.as_secs()
        ));
        if !escalate {
            return;
        }
        let notice = format!(
            "ESCALATION: Task #{} from {} cannot merge: the root {} checkout has uncommitted changes that the merge would overwrite.\nPaths: {paths}\nDeferred {deferrals} times so far. Commit, stash, or remove these changes in the root checkout; Batty keeps retrying the merge.\ngit: {}",
            request.task_id, request.engineer, dirty.trunk_branch, dirty.stderr
        );
        if let Some(manager_name) = self.manager_name(&request.engineer) {
            self.notify_best_effort("daemon", &manager_name, &notice);
            self.mark_member_working(&manager_name);
            self.notify_reports_to_best_effort(&manager_name, &notice);
        }
        self.record_task_escalated(
            &request.engineer,
            request.task_id.to_string(),
            Some("merge_deferred_dirty_root"),
        );
    }

    /// Clear the engineer's assignment once a queued merge is finished with
    /// — unless the engineer has moved on to another task (a chat merge of
    /// an older review task must not clobber the current assignment).
    fn release_engineer_from_merged_task(&mut self, request: &MergeRequest) {
        if self.engineer_still_owns(request) {
            self.clear_active_task(&request.engineer);
            self.set_member_idle(&request.engineer);
        }
    }

    /// False when the engineer's active task is a different task.
    fn engineer_still_owns(&self, request: &MergeRequest) -> bool {
        self.active_task_id(&request.engineer)
            .is_none_or(|task_id| task_id == request.task_id)
    }

    pub(crate) fn enqueue_merge_request(&mut self, request: MergeRequest) {
        info!(
            task_id = request.task_id,
            engineer = %request.engineer,
            "enqueuing merge request"
        );
        self.merge.queue.enqueue(request);
        self.persist_merge_queue_change();
    }

    /// Persist daemon state right away so queued, in-flight and deferred
    /// merges survive a crash between heartbeats.
    pub(in crate::team) fn persist_merge_queue_change(&self) {
        if let Err(error) = self.persist_runtime_state(false) {
            warn!(error = %error, "failed to persist merge queue state");
        }
    }

    /// Restore only the merge queue from the persisted daemon state (fresh
    /// starts that do not restore the rest of the runtime state), so a merge
    /// that landed but was never verified is not left on trunk silently.
    pub(in crate::team) fn restore_merge_queue_state(&mut self) {
        if let Some(state) = super::state::load_daemon_state(self.project_root()) {
            self.restore_merge_state(state.merge_queue, state.deferred_direct_merges);
        }
    }

    /// Restore the persisted merge queue and deferred direct merges, then
    /// re-take the merge lock for a restored in-flight merge.
    pub(super) fn restore_merge_state(
        &mut self,
        queue: PersistedMergeQueue,
        deferred_direct: Vec<DeferredDirectMerge>,
    ) {
        self.merge.queue.restore(queue);
        crate::team::merge::restore_deferred_direct_merges(self, deferred_direct);
        if self.merge.queue.is_verifying() {
            self.ensure_in_flight_merge_lock(Instant::now());
        } else if crate::team::merge::merge_lock_path(self.project_root()).exists() {
            // A lock file left by this daemon before an exec (hot reload)
            // still carries our live PID, so other processes would treat it
            // as held. Taking and releasing it clears it.
            if let Ok(Some(lock)) = MergeLock::try_acquire(self.project_root()) {
                drop(lock);
            }
        }
    }

    /// Make sure the in-flight merge holds the merge lock (it does not after
    /// a restore). Returns false while it cannot be taken. Lock errors are
    /// counted; after [`MERGE_LOCK_FAILURE_ESCALATE_AFTER`] in a row they are
    /// escalated once and retried every few minutes instead of every tick.
    fn ensure_in_flight_merge_lock(&mut self, now: Instant) -> bool {
        let Some(merge) = self.merge.queue.in_flight.as_ref() else {
            return false;
        };
        if merge.lock.is_some() {
            return true;
        }
        let task_id = merge.request.task_id;
        let engineer = merge.request.engineer.clone();
        if self
            .merge
            .queue
            .lock_retry_at
            .is_some_and(|retry_at| now < retry_at)
        {
            return false;
        }
        match MergeLock::try_acquire(self.project_root()) {
            Ok(Some(lock)) => {
                if let Some(merge) = self.merge.queue.in_flight.as_mut() {
                    merge.lock = Some(lock);
                }
                self.merge.queue.lock_failures = 0;
                self.merge.queue.lock_retry_at = None;
                true
            }
            Ok(None) => {
                debug!(
                    task_id,
                    "merge lock busy; in-flight merge waits for it on a later tick"
                );
                false
            }
            Err(error) => {
                self.record_in_flight_lock_failure(task_id, &engineer, &error, now);
                false
            }
        }
    }

    fn record_in_flight_lock_failure(
        &mut self,
        task_id: u32,
        engineer: &str,
        error: &anyhow::Error,
        now: Instant,
    ) {
        let queue = &mut self.merge.queue;
        queue.lock_failures = queue.lock_failures.saturating_add(1);
        let failures = queue.lock_failures;
        warn!(
            task_id,
            failures,
            error = %format!("{error:#}"),
            "failed to take the merge lock for the in-flight merge; will retry"
        );
        if failures < MERGE_LOCK_FAILURE_ESCALATE_AFTER {
            return;
        }
        queue.lock_retry_at = Some(now + MERGE_LOCK_FAILURE_RETRY);
        if failures != MERGE_LOCK_FAILURE_ESCALATE_AFTER {
            return;
        }
        let notice = format!(
            "ESCALATION: Batty could not take the merge lock ({}) for task #{task_id}'s post-merge verification or revert after {failures} attempts: {error:#}\nThe merge stays on trunk unverified. Batty retries every {} minutes; check the filesystem holding .batty/.",
            crate::team::merge::merge_lock_path(self.project_root()).display(),
            MERGE_LOCK_FAILURE_RETRY.as_secs() / 60
        );
        self.record_orchestrator_action(format!(
            "merge queue: merge lock failed {failures} times for task #{task_id} ({error:#}); escalated"
        ));
        if let Some(manager_name) = self.manager_name(engineer) {
            self.notify_best_effort("daemon", &manager_name, &notice);
            self.mark_member_working(&manager_name);
            self.notify_reports_to_best_effort(&manager_name, &notice);
        }
        self.record_task_escalated(
            engineer,
            task_id.to_string(),
            Some("merge_lock_unavailable"),
        );
    }

    fn notify_best_effort(&mut self, from: &str, recipient: &str, body: &str) {
        if let Err(error) = self.queue_message(from, recipient, body) {
            warn!(
                from,
                recipient,
                error = %error,
                "merge queue: failed to deliver notice"
            );
        }
    }

    fn notify_reports_to_best_effort(&mut self, from_role: &str, body: &str) {
        if let Err(error) = self.notify_reports_to(from_role, body) {
            warn!(
                from_role,
                error = %error,
                "merge queue: failed to notify reports-to"
            );
        }
    }

    fn post_merge_test_command(&self) -> Option<String> {
        let workflow = &self.config.team_config.workflow_policy;
        workflow
            .verification
            .test_command
            .as_deref()
            .or(workflow.test_command.as_deref())
            .map(str::to_string)
    }

    /// Run the post-merge test command against the trunk tip `post` (or
    /// the current trunk ref): in the root checkout when it has trunk
    /// checked out, otherwise in a dedicated detached verify worktree.
    fn spawn_post_merge_verification(
        &self,
        task_id: u32,
        post: Option<String>,
    ) -> TestJob<VerificationRunResult> {
        let project_root = self.project_root().to_path_buf();
        let trunk_branch = self.config.team_config.trunk_branch().to_string();
        let test_command = self.post_merge_test_command();
        TestJob::spawn(format!("post-merge-{task_id}"), move |child_pgid| {
            let dir = post_merge_verification_dir(&project_root, &trunk_branch, post.as_deref())
                .with_context(|| {
                    format!(
                        "failed to prepare a {trunk_branch} checkout for post-merge verification"
                    )
                })?;
            run_post_merge_tests(&dir, test_command.as_deref(), child_pgid.as_ref()).with_context(
                || format!("post-merge verification on {trunk_branch} failed to execute"),
            )
        })
    }

    /// Phase 2 of a verified merge: start an interrupted verification, apply
    /// a finished one, or retry a pending revert.
    fn advance_in_flight_merge(&mut self) {
        enum Next {
            Wait,
            Start(u32, Option<String>),
            Passed,
            Failed(String),
            RetryRevert,
        }
        if !self.ensure_in_flight_merge_lock(Instant::now()) {
            return;
        }
        let next = match self.merge.queue.in_flight.as_ref() {
            None => return,
            Some(merge) => match &merge.verify {
                PostMergeVerify::Pending => {
                    if self.verification.background_tests.main_smoke.is_some() {
                        Next::Wait
                    } else {
                        Next::Start(merge.request.task_id, merge.post_merge_head.clone())
                    }
                }
                PostMergeVerify::Running(job) => match job.try_take() {
                    None => Next::Wait,
                    Some(Ok(run)) if run.passed => Next::Passed,
                    Some(Ok(run)) => Next::Failed(run.output),
                    // Spawn failures, worker panics and cancellations all
                    // count as failed verification: never land unverified.
                    Some(Err(error)) => {
                        Next::Failed(format!("post-merge verification could not run: {error:#}"))
                    }
                },
                PostMergeVerify::Failed(_) => Next::RetryRevert,
            },
        };
        match next {
            Next::Wait => {}
            Next::Start(task_id, post) => {
                let job = self.spawn_post_merge_verification(task_id, post);
                if let Some(merge) = self.merge.queue.in_flight.as_mut() {
                    merge.verify = PostMergeVerify::Running(job);
                }
                self.record_orchestrator_action(format!(
                    "merge queue: re-running interrupted post-merge verification for task #{task_id}"
                ));
            }
            Next::Passed => {
                let Some(merge) = self.merge.queue.take_in_flight() else {
                    return;
                };
                let outcome = self.complete_landed_merge(
                    &merge.request,
                    merge.mode,
                    merge.merge_reason.as_deref(),
                    Some(true),
                );
                self.merge
                    .queue
                    .record_result(merge.request.task_id, outcome);
            }
            Next::Failed(output) => {
                if let Some(merge) = self.merge.queue.in_flight.as_mut() {
                    merge.verify = PostMergeVerify::Failed(output);
                }
                self.revert_failed_in_flight_merge();
            }
            Next::RetryRevert => self.revert_failed_in_flight_merge(),
        }
    }

    /// Undo a merge whose post-merge verification failed. Runs under the
    /// merge lock held by the in-flight merge.
    fn revert_failed_in_flight_merge(&mut self) {
        let trunk_branch = self.config.team_config.trunk_branch().to_string();
        let Some(merge) = self.merge.queue.in_flight.as_ref() else {
            return;
        };
        let result = apply_post_merge_revert(
            self.project_root(),
            &trunk_branch,
            &merge.pre_merge_head,
            merge.post_merge_head.as_deref(),
        );
        let (reverted_by_us, manual_reason) = match result {
            RevertResult::Reverted => (true, None),
            RevertResult::AlreadyReverted => (false, None),
            RevertResult::Manual(reason) => (false, Some(reason)),
            RevertResult::Pending { reason, paths } => {
                // Keep the merge (and the lock) in flight; retried next tick.
                self.escalate_pending_revert(&trunk_branch, &reason, &paths);
                return;
            }
        };
        let Some(mut merge) = self.merge.queue.take_in_flight() else {
            return;
        };
        drop(merge.lock.take());
        let output = match &merge.verify {
            PostMergeVerify::Failed(output) => output.clone(),
            _ => String::new(),
        };
        let request = &merge.request;
        self.record_auto_merge_post_verify_result(
            &request.engineer,
            request.task_id,
            Some(false),
            "failed",
            Some("post-merge verification on main failed"),
        );

        let manager_name = self.manager_name(&request.engineer);
        let task_title = read_task_title(&self.board_dir(), request.task_id);
        let outcome = match manual_reason {
            None => {
                let what = if reverted_by_us {
                    format!("so Batty reverted {trunk_branch} to the pre-merge commit")
                } else {
                    format!(
                        "and {trunk_branch} was already back at the pre-merge commit (reverted outside Batty)"
                    )
                };
                let engineer_notice = format!(
                    "Your task for #{} merged cleanly but failed post-merge verification on {trunk_branch}, {what}.\nLatest output:\n{}",
                    request.task_id, output
                );
                self.notify_best_effort("daemon", &request.engineer, &engineer_notice);
                self.mark_member_working(&request.engineer);
                if let Some(ref manager_name) = manager_name {
                    let manager_notice = format!(
                        "[{}] Task #{} failed post-merge verification on {trunk_branch}, {what}.\nTitle: {}\nLatest output:\n{}",
                        request.engineer, request.task_id, task_title, output
                    );
                    self.notify_best_effort("daemon", manager_name, &manager_notice);
                    self.mark_member_working(manager_name);
                }
                self.record_orchestrator_action(format!(
                    "merge queue: task #{} failed post-merge verification; {}",
                    request.task_id,
                    if reverted_by_us {
                        format!("reverted {trunk_branch} to {}", merge.pre_merge_head)
                    } else {
                        format!("{trunk_branch} was already at {}", merge.pre_merge_head)
                    }
                ));
                MergeQueueOutcome::Reverted
            }
            Some(reason) => {
                warn!(
                    task_id = request.task_id,
                    reason = %reason,
                    "post-merge verification failed and the merge could not be reverted automatically"
                );
                self.record_orchestrator_action(format!(
                    "merge queue: task #{} failed post-merge verification but was NOT reverted ({reason}); manual revert required",
                    request.task_id
                ));
                let notice = format!(
                    "ESCALATION: Task #{} from {} failed post-merge verification on {trunk_branch}, but Batty could not revert it automatically: {reason}.\nTitle: {}\nPre-merge head: {}\nRevert the merge manually.\nLatest output:\n{}",
                    request.task_id, request.engineer, task_title, merge.pre_merge_head, output
                );
                if let Some(ref manager_name) = manager_name {
                    self.notify_best_effort("daemon", manager_name, &notice);
                    self.mark_member_working(manager_name);
                    self.notify_reports_to_best_effort(manager_name, &notice);
                }
                let engineer_notice = format!(
                    "Your task for #{} failed post-merge verification on {trunk_branch}. Batty could not revert it automatically ({reason}); wait for lead direction.\nLatest output:\n{}",
                    request.task_id, output
                );
                self.notify_best_effort("daemon", &request.engineer, &engineer_notice);
                self.record_task_escalated(
                    &request.engineer,
                    request.task_id.to_string(),
                    Some("post_merge_revert_failed"),
                );
                MergeQueueOutcome::Failed
            }
        };
        self.merge.queue.record_result(request.task_id, outcome);
    }

    /// The revert cannot run yet (the root checkout has uncommitted source
    /// changes). Escalate once; the revert stays pending and is retried.
    fn escalate_pending_revert(&mut self, trunk_branch: &str, reason: &str, paths: &[String]) {
        let Some(merge) = self.merge.queue.in_flight.as_mut() else {
            return;
        };
        if merge.dirty_revert_escalated {
            debug!(
                task_id = merge.request.task_id,
                reason, "post-merge revert still waiting"
            );
            return;
        }
        merge.dirty_revert_escalated = true;
        let request = merge.request.clone();
        let pre = merge.pre_merge_head.clone();
        let paths = if paths.is_empty() {
            "(none listed)".to_string()
        } else {
            paths.join(", ")
        };
        warn!(
            task_id = request.task_id,
            reason,
            paths = %paths,
            "post-merge revert deferred: root checkout has uncommitted changes"
        );
        self.record_orchestrator_action(format!(
            "merge queue: revert of task #{} is waiting ({reason}; paths: {paths})",
            request.task_id
        ));
        let notice = format!(
            "ESCALATION: Task #{} from {} failed post-merge verification on {trunk_branch}, but Batty has not reverted it: {reason}.\nPaths: {paths}\nPre-merge commit: {pre}\nCommit or stash those changes in the root checkout and Batty reverts {trunk_branch} to {pre} automatically, or revert the merge yourself.",
            request.task_id, request.engineer
        );
        if let Some(manager_name) = self.manager_name(&request.engineer) {
            self.notify_best_effort("daemon", &manager_name, &notice);
            self.mark_member_working(&manager_name);
            self.notify_reports_to_best_effort(&manager_name, &notice);
        }
        self.record_task_escalated(
            &request.engineer,
            request.task_id.to_string(),
            Some("post_merge_revert_blocked_dirty_root"),
        );
    }

    /// Bookkeeping after a merge has landed (and passed verification, when
    /// requested). Nothing here may abort: the merge is on trunk, so the
    /// board update and member state changes must always run.
    fn complete_landed_merge(
        &mut self,
        request: &MergeRequest,
        mode: MergeMode,
        merge_reason: Option<&str>,
        verified: Option<bool>,
    ) -> MergeQueueOutcome {
        let trunk_branch = self.config.team_config.trunk_branch().to_string();
        let board_dir = self.board_dir();
        let board_dir_str = board_dir.to_string_lossy().to_string();
        let manager_name = self.manager_name(&request.engineer);
        let task_title = read_task_title(&board_dir, request.task_id);

        match verified {
            Some(true) => self.record_auto_merge_post_verify_result(
                &request.engineer,
                request.task_id,
                Some(true),
                "passed",
                Some("post-merge verification on main passed"),
            ),
            _ => self.record_auto_merge_post_verify_result(
                &request.engineer,
                request.task_id,
                None,
                "skipped",
                Some("post-merge verification was not requested for this merge"),
            ),
        }

        let board_update_ok = match request.origin {
            MergeOrigin::Chat => approve_chat_merge(self, &board_dir, &board_dir_str, request),
            MergeOrigin::AutoMerge | MergeOrigin::DirectCompletion => move_task_to_done(
                self,
                &board_dir,
                &board_dir_str,
                request,
                manager_name.as_deref(),
            ),
        };
        if let Err(error) = crate::team::merge::record_merge_test_timing(
            self,
            request.task_id,
            &request.engineer,
            &request.branch,
            request.test_duration_ms,
        ) {
            warn!(
                engineer = request.engineer,
                task_id = request.task_id,
                error = %error,
                "failed to record merge test timing"
            );
        }
        match request.origin {
            MergeOrigin::AutoMerge => self.record_task_auto_merged(
                &request.engineer,
                request.task_id,
                request.confidence,
                request.files_changed,
                request.lines_changed,
                mode,
            ),
            MergeOrigin::DirectCompletion => {
                self.record_task_manual_merged(request.task_id, mode);
                crate::team::merge::append_merge_learning(
                    self,
                    &board_dir,
                    request.task_id,
                    &request.engineer,
                );
            }
            MergeOrigin::Chat => self.record_task_manual_merged(request.task_id, mode),
        }
        self.check_binary_freshness_after_merge();

        if let Some(ref manager_name) = manager_name {
            let msg = format!(
                "[{}] Task #{} completed from merge queue.\nTitle: {}\nTests: passed\nMerge: success{}{}{}",
                request.engineer,
                request.task_id,
                task_title,
                if mode == MergeMode::IsolatedIntegration {
                    "\nMerge mode: isolated integration checkout"
                } else {
                    ""
                },
                if let Some(reason) = merge_reason {
                    format!("\nMerge reason: {reason}")
                } else {
                    String::new()
                },
                if board_update_ok {
                    ""
                } else {
                    "\nBoard: update failed; decide next board action manually."
                }
            );
            self.notify_best_effort(&request.engineer, manager_name, &msg);
            self.mark_member_working(manager_name);
            let rollup = format!(
                "Rollup: Task #{} completed by {} from the merge queue. Tests passed, merged to {trunk_branch}.{}",
                request.task_id,
                request.engineer,
                if board_update_ok {
                    ""
                } else {
                    " Board automation failed; decide manually."
                }
            );
            self.notify_reports_to_best_effort(manager_name, &rollup);
        }

        // Post-merge disk hygiene: clean build artifacts and prune branch
        let hygiene_config = &self.config.team_config.automation.disk_hygiene;
        let hygiene_report = super::health::disk_hygiene::post_merge_cleanup(
            self.project_root(),
            &request.engineer,
            request.task_id,
            &request.branch,
            hygiene_config,
        );
        if hygiene_report.any_action_taken() {
            let summary = hygiene_report.summary();
            info!(
                engineer = request.engineer,
                task_id = request.task_id,
                summary = %summary,
                "post-merge disk hygiene"
            );
            self.record_orchestrator_action(format!(
                "disk-hygiene: post-merge cleanup for {} task #{}: {summary}",
                request.engineer, request.task_id
            ));
        }

        let owns_task = self.engineer_still_owns(request);
        if owns_task {
            self.clear_active_task(&request.engineer);
        }
        self.record_task_completed(&request.engineer, Some(request.task_id));
        if owns_task {
            self.set_member_idle(&request.engineer);
        }
        info!(
            engineer = request.engineer,
            task_id = request.task_id,
            "merge queue processed request successfully"
        );
        MergeQueueOutcome::Success
    }

    /// Phase 1 of a queued merge: checks, then merge under the merge lock.
    /// Errors returned here happen before anything lands on trunk, so the
    /// queue retries them.
    fn execute_queued_merge(&mut self, request: &MergeRequest) -> Result<MergeAttempt> {
        let trunk_branch = self.config.team_config.trunk_branch().to_string();
        if self.is_multi_repo {
            bail!("merge queue execution is not yet implemented for multi-repo projects");
        }

        if let Some((reason, detail)) = merge_request_skip_reason(
            self.project_root(),
            request,
            self.config.team_config.trunk_branch(),
        )? {
            warn!(
                task_id = request.task_id,
                engineer = request.engineer,
                reason = reason.as_str(),
                detail = %detail,
                "skipping daemon auto-merge request"
            );
            if reason == AutoMergeSkipReason::MissingPacket
                && detail.contains("verification_retry_required")
            {
                if self.engineer_still_owns(request) {
                    self.redispatch_verification_retry_required(request, &detail)?;
                } else {
                    // The engineer moved on to another task: never replace
                    // their current assignment from a stale merge request.
                    self.record_orchestrator_action(format!(
                        "merge queue: task #{} needs another verification pass, but {} is working on another task; left in review for the manager",
                        request.task_id, request.engineer
                    ));
                }
            }
            self.record_orchestrator_action(format!(
                "merge queue: skipped auto-merge for task #{} ({reason}: {detail})",
                request.task_id,
                reason = reason.as_str()
            ));
            return Ok(MergeAttempt::Done(MergeQueueOutcome::Skipped));
        }

        let Some(lock) =
            MergeLock::try_acquire(self.project_root()).context("failed to acquire merge lock")?
        else {
            return Ok(MergeAttempt::LockBusy);
        };
        let board_dir = self.board_dir();
        let board_dir_str = board_dir.to_string_lossy().to_string();
        let manager_name = self.manager_name(&request.engineer);
        let task_title = read_task_title(&board_dir, request.task_id);
        let root_dirty = inspect_root_dirty_state(self.project_root())?;
        if let Some(block_detail) = dirty_main_review_merge_block_detail(&root_dirty) {
            drop(lock);
            let notice = format!(
                "merge queue: blocked auto-merge for task #{} because main has source edits. {block_detail}",
                request.task_id
            );
            self.record_orchestrator_action(notice.clone());
            mark_review_merge_blocked(&board_dir, request.task_id, &block_detail);

            let manager_notice = format!(
                "Task #{} from {} passed tests but cannot be auto-merged while main has dirty source edits.\nTitle: {}\n{}\nLeave the task in review and clean, commit, or stash the root changes before retrying.",
                request.task_id, request.engineer, task_title, block_detail
            );
            if let Some(ref manager_name) = manager_name {
                self.notify_best_effort("daemon", manager_name, &manager_notice);
                self.mark_member_working(manager_name);
                self.notify_reports_to_best_effort(manager_name, &manager_notice);
            }

            let engineer_notice = format!(
                "Your task passed tests, but Batty paused the merge because main has dirty source edits.\n{}\nWait for lead direction before making more changes.",
                block_detail
            );
            self.notify_best_effort("daemon", &request.engineer, &engineer_notice);

            warn!(
                engineer = request.engineer,
                task_id = request.task_id,
                dirty_paths = %root_dirty.source_paths.join(", "),
                "merge queue blocked by dirty main source changes"
            );
            return Ok(MergeAttempt::Done(MergeQueueOutcome::Skipped));
        }
        if root_dirty.is_runtime_only() {
            match snapshot_runtime_dirty_main(self.project_root(), request, &task_title, &root_dirty)
            {
                Ok(Some(snapshot_path)) => self.record_orchestrator_action(format!(
                    "merge queue: preserved runtime-only dirty main state before task #{} merge ({snapshot_path})",
                    request.task_id
                )),
                Ok(None) => {}
                Err(error) => {
                    warn!(
                        task_id = request.task_id,
                        error = %error,
                        "failed to snapshot runtime-only dirty main state before merge"
                    );
                    self.record_orchestrator_action(format!(
                        "merge queue: runtime-only dirty main snapshot failed before task #{} merge ({error})",
                        request.task_id
                    ));
                }
            }
        }
        // Record the trunk ref itself, not the root HEAD: in isolated mode
        // the root may have another branch checked out.
        let pre_merge_head = trunk_head(self.project_root(), &trunk_branch)?;

        let merge_outcome = match merge_engineer_branch_into_trunk(
            self.project_root(),
            &request.engineer,
            &trunk_branch,
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let dirty = error.downcast::<RootCheckoutDirty>()?;
                drop(lock);
                return Ok(MergeAttempt::DirtyRootDeferred(dirty));
            }
        };
        match merge_outcome {
            MergeOutcome::Success(success) => {
                // The merge has landed: from here on nothing may return an
                // error, or the queue would retry an already-merged request.
                let post_merge_head = match trunk_head(self.project_root(), &trunk_branch) {
                    Ok(head) => Some(head),
                    Err(error) => {
                        warn!(
                            task_id = request.task_id,
                            error = %error,
                            "failed to record the post-merge trunk ref; a failed verification will need a manual revert"
                        );
                        None
                    }
                };
                if success.mode == MergeMode::IsolatedIntegration {
                    let reason = success
                        .reason
                        .as_deref()
                        .unwrap_or("root checkout required isolation");
                    self.record_orchestrator_action(format!(
                        "merge queue: isolated merge for task #{} ({reason})",
                        request.task_id
                    ));
                }
                if request.should_post_merge_verify
                    && self
                        .config
                        .team_config
                        .workflow_policy
                        .auto_merge
                        .post_merge_verify
                {
                    let job = self
                        .spawn_post_merge_verification(request.task_id, post_merge_head.clone());
                    // The lock stays held until the merge is finalized or
                    // reverted.
                    return Ok(MergeAttempt::Verifying(Box::new(InFlightMerge::new(
                        request.clone(),
                        pre_merge_head,
                        post_merge_head,
                        success.mode,
                        success.reason,
                        PostMergeVerify::Running(job),
                        Some(lock),
                    ))));
                }
                drop(lock);
                Ok(MergeAttempt::Done(self.complete_landed_merge(
                    request,
                    success.mode,
                    success.reason.as_deref(),
                    None,
                )))
            }
            MergeOutcome::RebaseConflict(conflict_info) => {
                drop(lock);
                let attempt = self.increment_retry(&request.engineer);
                if attempt <= 2 {
                    let msg = format!(
                        "Merge conflict during rebase onto {trunk_branch} (attempt {attempt}/2). Fix the conflicts in your worktree and try again:\n{conflict_info}"
                    );
                    self.notify_best_effort("batty", &request.engineer, &msg);
                    self.mark_member_working(&request.engineer);
                } else {
                    if let Some(ref manager_name) = manager_name {
                        let msg = format!(
                            "[{}] task #{} has unresolvable merge conflicts after 2 retries. Escalating.\n{}",
                            request.engineer, request.task_id, conflict_info
                        );
                        self.notify_best_effort(&request.engineer, manager_name, &msg);
                        self.mark_member_working(manager_name);
                        let escalation = format!(
                            "ESCALATION: Task #{} assigned to {} has unresolvable merge conflicts. Task blocked on board.",
                            request.task_id, request.engineer
                        );
                        self.notify_reports_to_best_effort(manager_name, &escalation);
                    }

                    self.record_task_escalated(
                        &request.engineer,
                        request.task_id.to_string(),
                        Some("merge_conflict"),
                    );
                    self.run_kanban_md_nonfatal(
                        &[
                            "edit",
                            &request.task_id.to_string(),
                            "--block",
                            "merge conflicts after 2 retries",
                            "--dir",
                            &board_dir_str,
                        ],
                        &format!(
                            "block task #{} after merge conflict retries",
                            request.task_id
                        ),
                        manager_name
                            .as_deref()
                            .into_iter()
                            .chain(std::iter::once(request.engineer.as_str())),
                    );
                    self.release_engineer_from_merged_task(request);
                }

                warn!(
                    engineer = request.engineer,
                    task_id = request.task_id,
                    "merge queue encountered a rebase conflict"
                );
                Ok(MergeAttempt::Done(MergeQueueOutcome::Conflict))
            }
            MergeOutcome::MergeFailure(merge_info) => {
                drop(lock);
                self.record_task_merge_failed(
                    &request.engineer,
                    request.task_id,
                    infer_merge_mode_from_failure(&merge_info),
                    &merge_info,
                );
                let manager_notice = format!(
                    "Task #{} from {} passed tests but could not be merged to {trunk_branch}.\n{}\nDecide whether to retry the isolated merge path, inspect the integration-worktree failure, or redirect the engineer.",
                    request.task_id, request.engineer, merge_info
                );
                if let Some(ref manager_name) = manager_name {
                    self.notify_best_effort("daemon", manager_name, &manager_notice);
                    self.mark_member_working(manager_name);
                    self.notify_reports_to_best_effort(manager_name, &manager_notice);
                }

                let engineer_notice = format!(
                    "Your task passed tests, but Batty could not merge it into {trunk_branch}.\n{}\nWait for lead direction before making more changes.",
                    merge_info
                );
                self.notify_best_effort("daemon", &request.engineer, &engineer_notice);

                self.record_task_escalated(
                    &request.engineer,
                    request.task_id.to_string(),
                    Some("merge_failure"),
                );
                self.release_engineer_from_merged_task(request);
                warn!(
                    engineer = request.engineer,
                    task_id = request.task_id,
                    error = %merge_info,
                    "merge queue failed to merge request"
                );
                Ok(MergeAttempt::Done(MergeQueueOutcome::Failed))
            }
        }
    }
}

/// Task id of a merge the (possibly running) daemon has landed but not yet
/// finished verifying, read from its persisted state. Lets out-of-process
/// merges refuse to land on trunk underneath it.
pub(crate) fn persisted_in_flight_merge(project_root: &std::path::Path) -> Option<u32> {
    super::state::load_daemon_state(project_root)?
        .merge_queue
        .in_flight
        .map(|merge| merge.request.task_id)
}

fn merge_request_skip_reason(
    project_root: &std::path::Path,
    request: &MergeRequest,
    trunk_branch: &str,
) -> Result<Option<(AutoMergeSkipReason, String)>> {
    let task = load_tasks_from_dir(
        &project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
    )?
    .into_iter()
    .find(|task| task.id == request.task_id);

    let Some(task) = task else {
        return Ok(Some((
            AutoMergeSkipReason::WrongStatus,
            "task is missing from the board".to_string(),
        )));
    };

    if task.status != "review" {
        return Ok(Some((
            AutoMergeSkipReason::WrongStatus,
            format!("task status is '{}' instead of 'review'", task.status),
        )));
    }

    let metadata = read_workflow_metadata(&task.source_path)?;
    if let Some(detail) = missing_completion_packet_detail(project_root, request, &metadata) {
        return Ok(Some((AutoMergeSkipReason::MissingPacket, detail)));
    }

    if let Some(detail) = unavailable_branch_detail(request, trunk_branch)? {
        return Ok(Some((AutoMergeSkipReason::NoBranch, detail)));
    }

    Ok(None)
}

impl TeamDaemon {
    fn redispatch_verification_retry_required(
        &mut self,
        request: &MergeRequest,
        detail: &str,
    ) -> Result<()> {
        let board_dir = self.board_dir();
        crate::team::task_cmd::transition_task_with_attribution(
            &board_dir,
            request.task_id,
            "in-progress",
            crate::team::task_cmd::StatusTransitionAttribution::daemon(
                "daemon.merge_queue.verification_retry",
            ),
        )?;
        crate::team::task_cmd::assign_task_owners(
            &board_dir,
            request.task_id,
            Some(&request.engineer),
            Some(""),
        )?;

        let message = format!(
            "Task #{} requires another verification pass.\n\nBatty could not merge the previous completion because {detail}.\n\nContinue on branch `{}` in `{}` and fix the verification failure before reporting completion again.",
            request.task_id,
            request.branch,
            request.worktree_dir.display()
        );
        let can_deliver_assignment = (self.config.team_config.use_shim
            && self.shim_handles.contains_key(&request.engineer))
            || self.config.pane_map.contains_key(&request.engineer);
        if can_deliver_assignment {
            match self.assign_task_with_task_id_as(
                "batty",
                &request.engineer,
                &message,
                Some(request.task_id),
            ) {
                Ok(_) => {
                    self.record_orchestrator_action(format!(
                        "merge queue: re-dispatched verification retry for task #{} to {}",
                        request.task_id, request.engineer
                    ));
                    return Ok(());
                }
                Err(error) => {
                    warn!(
                        task_id = request.task_id,
                        engineer = %request.engineer,
                        error = %error,
                        "verification retry assignment launch failed; falling back to inbox delivery"
                    );
                }
            }
        }

        self.queue_message("batty", &request.engineer, &message)?;
        self.active_tasks
            .insert(request.engineer.clone(), request.task_id);
        self.mark_member_working(&request.engineer);
        self.record_orchestrator_action(format!(
            "merge queue: queued verification retry for task #{} to {}",
            request.task_id, request.engineer
        ));
        Ok(())
    }
}

fn dirty_main_review_merge_block_detail(root_dirty: &RootDirtyState) -> Option<String> {
    if root_dirty.source_paths.is_empty() {
        return None;
    }

    let staged_source = root_dirty
        .staged_paths
        .iter()
        .filter(|path| root_dirty.source_paths.contains(path))
        .cloned()
        .collect::<Vec<_>>();
    let unstaged_source = root_dirty
        .unstaged_paths
        .iter()
        .filter(|path| root_dirty.source_paths.contains(path))
        .cloned()
        .collect::<Vec<_>>();
    let mut parts = vec![format!(
        "Dirty source paths: {}.",
        root_dirty.source_paths.join(", ")
    )];
    if !staged_source.is_empty() {
        parts.push(format!("Staged: {}.", staged_source.join(", ")));
    }
    if !unstaged_source.is_empty() {
        parts.push(format!(
            "Unstaged/untracked: {}.",
            unstaged_source.join(", ")
        ));
    }
    parts.push(
        "Next action: commit, stash, or clean these root worktree changes before retrying the review merge."
            .to_string(),
    );
    Some(parts.join(" "))
}

fn mark_review_merge_blocked(board_dir: &std::path::Path, task_id: u32, block_detail: &str) {
    let mut fields = HashMap::new();
    fields.insert("blocked_on".to_string(), block_detail.to_string());
    fields.insert("block_reason".to_string(), block_detail.to_string());
    if let Err(error) = crate::team::task_cmd::cmd_update(board_dir, task_id, fields) {
        warn!(
            task_id,
            error = %error,
            "failed to record dirty-main merge block on review task"
        );
    }
}

fn snapshot_runtime_dirty_main(
    project_root: &std::path::Path,
    request: &MergeRequest,
    task_title: &str,
    root_dirty: &RootDirtyState,
) -> Result<Option<String>> {
    if !root_dirty.is_runtime_only() {
        return Ok(None);
    }

    let task = load_tasks_from_dir(
        &project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
    )?
    .into_iter()
    .find(|task| task.id == request.task_id)
    .unwrap_or_else(|| fallback_snapshot_task(request, task_title));
    let source_head = git_head(project_root).ok();
    let reason = format!(
        "runtime-only dirty main before review merge; paths: {}",
        root_dirty.runtime_paths.join(", ")
    );
    let snapshot_path = crate::team::checkpoint::write_dirty_lane_snapshot(
        project_root,
        project_root,
        "main",
        &task,
        "main",
        &request.branch,
        &reason,
        source_head.as_deref(),
    )?;
    let snapshot_relative = snapshot_path
        .strip_prefix(project_root)
        .unwrap_or(&snapshot_path)
        .to_string_lossy()
        .to_string();
    let record = crate::team::checkpoint::PreservedLaneRecord::snapshot(
        "main",
        &task,
        "main",
        &request.branch,
        &reason,
        source_head,
        snapshot_relative.clone(),
    );
    crate::team::checkpoint::write_preserved_lane_record(project_root, &record)?;
    Ok(Some(snapshot_relative))
}

fn fallback_snapshot_task(request: &MergeRequest, task_title: &str) -> crate::task::Task {
    crate::task::Task {
        id: request.task_id,
        title: task_title.to_string(),
        status: "review".to_string(),
        priority: "high".to_string(),
        assignee: None,
        claimed_by: Some(request.engineer.clone()),
        claimed_at: None,
        claim_ttl_secs: None,
        claim_expires_at: None,
        last_progress_at: None,
        claim_warning_sent_at: None,
        claim_extensions: None,
        last_output_bytes: None,
        blocked: None,
        tags: Vec::new(),
        depends_on: Vec::new(),
        review_owner: None,
        blocked_on: None,
        worktree_path: None,
        branch: Some(request.branch.clone()),
        commit: None,
        artifacts: Vec::new(),
        next_action: None,
        scheduled_for: None,
        cron_schedule: None,
        cron_last_run: None,
        completed: None,
        description: String::new(),
        batty_config: None,
        source_path: PathBuf::new(),
    }
}

fn missing_completion_packet_detail(
    project_root: &std::path::Path,
    request: &MergeRequest,
    metadata: &WorkflowMetadata,
) -> Option<String> {
    let mut missing = Vec::new();

    match metadata
        .branch
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        Some(branch) if branch == request.branch => {}
        Some(branch) => missing.push(format!(
            "branch marker '{}' does not match queued branch '{}'",
            branch, request.branch
        )),
        None => missing.push("branch marker missing".to_string()),
    }

    if metadata
        .commit
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        missing.push("commit marker missing".to_string());
    }

    match metadata
        .worktree_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        Some(path) => {
            let resolved = resolve_project_path(project_root, path);
            if resolved != request.worktree_dir {
                missing.push(format!(
                    "worktree marker '{}' resolves to '{}' instead of '{}'",
                    path,
                    resolved.display(),
                    request.worktree_dir.display()
                ));
            }
        }
        None => missing.push("worktree marker missing".to_string()),
    }

    if metadata.tests_run != Some(true) {
        missing.push("tests_run marker is not true".to_string());
    }
    if metadata.tests_passed != Some(true) {
        missing.push("tests_passed marker is not true".to_string());
    }
    if !metadata.review_blockers.is_empty() {
        missing.push(format!(
            "review blockers present: {}",
            metadata.review_blockers.join(", ")
        ));
    }
    match metadata
        .outcome
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some("verification_retry_required" | "verification_escalated") => {
            missing.push(format!(
                "outcome marker '{}' is not merge-ready",
                metadata.outcome.as_deref().unwrap_or_default()
            ));
        }
        Some(_) => {}
        None => missing.push("outcome marker missing".to_string()),
    }

    (!missing.is_empty()).then(|| missing.join("; "))
}

fn resolve_project_path(project_root: &std::path::Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

fn unavailable_branch_detail(request: &MergeRequest, trunk_branch: &str) -> Result<Option<String>> {
    if !request.worktree_dir.exists() {
        return Ok(Some(format!(
            "worktree '{}' does not exist",
            request.worktree_dir.display()
        )));
    }

    let current_branch = match current_worktree_branch(&request.worktree_dir) {
        Ok(branch) => branch,
        Err(error) => {
            return Ok(Some(format!(
                "failed to read worktree branch at '{}': {error}",
                request.worktree_dir.display()
            )));
        }
    };
    if current_branch != request.branch {
        return Ok(Some(format!(
            "worktree branch is '{}' instead of '{}'",
            current_branch, request.branch
        )));
    }

    let commits_ahead = match commits_ahead_of_trunk(&request.worktree_dir, trunk_branch) {
        Ok(commits) => commits,
        Err(error) => {
            return Ok(Some(format!(
                "failed to count commits ahead of {trunk_branch} for '{}': {error}",
                request.branch
            )));
        }
    };
    if commits_ahead == 0 {
        return Ok(Some(format!(
            "branch '{}' has no commits ahead of {trunk_branch}",
            request.branch
        )));
    }

    Ok(None)
}

fn commits_ahead_of_trunk(worktree_dir: &std::path::Path, trunk_branch: &str) -> Result<u32> {
    let range = format!("{trunk_branch}..HEAD");
    let output = crate::git::cmd(worktree_dir)
        .args(["rev-list", "--count", &range])
        .allow_fail()
        .run()
        .with_context(|| {
            format!(
                "failed to count commits ahead of {trunk_branch} in {}",
                worktree_dir.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "git rev-list --count {range} failed in {}: {}",
            worktree_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .with_context(|| {
            format!(
                "failed to parse git rev-list --count {range} output in {}",
                worktree_dir.display()
            )
        })
}

fn git_head(repo_dir: &std::path::Path) -> Result<String> {
    let output = crate::git::cmd(repo_dir)
        .args(["rev-parse", "HEAD"])
        .allow_fail()
        .run()
        .with_context(|| {
            format!(
                "failed to capture pre-merge HEAD for post-merge verification in {}",
                repo_dir.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "failed to capture pre-merge HEAD in {}: {}",
            repo_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_output_text(output: &crate::git::GitOutput) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

/// The commit `refs/heads/<trunk_branch>` points at.
fn trunk_head(repo_dir: &std::path::Path, trunk_branch: &str) -> Result<String> {
    let spec = format!("refs/heads/{trunk_branch}^{{commit}}");
    let output = crate::git::cmd(repo_dir)
        .args(["rev-parse", "--verify", "--quiet", &spec])
        .allow_fail()
        .run()
        .with_context(|| format!("failed to read {trunk_branch} in {}", repo_dir.display()))?;
    if !output.status.success() {
        bail!(
            "failed to read refs/heads/{trunk_branch} in {}: {}",
            repo_dir.display(),
            git_output_text(&output)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// True when the checkout at `repo_dir` has `branch` checked out.
fn checks_out_branch(repo_dir: &std::path::Path, branch: &str) -> bool {
    crate::git::cmd(repo_dir)
        .args(["symbolic-ref", "-q", "HEAD"])
        .allow_fail()
        .run()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == format!("refs/heads/{branch}")
        })
}

/// Result of undoing a merge whose post-merge verification failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RevertResult {
    /// The trunk ref was moved back to the pre-merge commit.
    Reverted,
    /// The trunk ref was already at the pre-merge commit.
    AlreadyReverted,
    /// Not reverted yet (the root checkout has uncommitted source changes);
    /// retry later.
    Pending { reason: String, paths: Vec<String> },
    /// Cannot be reverted automatically.
    Manual(String),
}

/// Move `refs/heads/<trunk_branch>` from `post` back to `pre`.
///
/// When the root checkout has trunk checked out, the root is reset with
/// `git reset --keep` so its index and files follow the ref; that is
/// skipped (left pending) while the root has uncommitted source changes,
/// and `--keep` itself refuses rather than discard local changes. Otherwise
/// only the ref moves, with `update-ref <pre> <post>`, which applies only if
/// the ref still points at `post`.
fn apply_post_merge_revert(
    project_root: &std::path::Path,
    trunk_branch: &str,
    pre: &str,
    post: Option<&str>,
) -> RevertResult {
    match plan_post_merge_revert(trunk_head(project_root, trunk_branch), pre, post) {
        RevertPlan::AlreadyReverted => return RevertResult::AlreadyReverted,
        RevertPlan::Manual(reason) => return RevertResult::Manual(reason),
        RevertPlan::Reset => {}
    }
    let Some(post) = post else {
        return RevertResult::Manual("the merge result commit was not recorded".to_string());
    };
    if !checks_out_branch(project_root, trunk_branch) {
        return match update_trunk_ref(project_root, trunk_branch, pre, post) {
            Ok(()) => RevertResult::Reverted,
            Err(error) => RevertResult::Manual(format!("{error:#}")),
        };
    }
    match inspect_root_dirty_state(project_root) {
        Ok(dirty) if !dirty.source_paths.is_empty() => {
            return RevertResult::Pending {
                reason: format!("the root {trunk_branch} checkout has uncommitted source changes"),
                paths: dirty.source_paths,
            };
        }
        Ok(_) => {}
        Err(error) => {
            return RevertResult::Pending {
                reason: format!("the root checkout could not be inspected ({error:#})"),
                paths: Vec::new(),
            };
        }
    }
    match reset_root_keep(project_root, pre) {
        Ok(()) => RevertResult::Reverted,
        Err(error) => RevertResult::Manual(format!("{error:#}")),
    }
}

fn update_trunk_ref(
    repo_dir: &std::path::Path,
    trunk_branch: &str,
    target: &str,
    expected: &str,
) -> Result<()> {
    let refname = format!("refs/heads/{trunk_branch}");
    let output = crate::git::cmd(repo_dir)
        .args(["update-ref", &refname, target, expected])
        .allow_fail()
        .run()
        .with_context(|| format!("failed to run git update-ref {refname}"))?;
    if !output.status.success() {
        bail!(
            "git update-ref {refname} {target} {expected} failed: {}",
            git_output_text(&output)
        );
    }
    Ok(())
}

/// `git reset --keep <target>` in the root: refuses (changing nothing)
/// instead of discarding local changes to files the reset would touch.
fn reset_root_keep(repo_dir: &std::path::Path, target: &str) -> Result<()> {
    let output = crate::git::cmd(repo_dir)
        .args(["reset", "--keep", target])
        .allow_fail()
        .run()
        .with_context(|| {
            format!(
                "failed to run git reset --keep {target} in {}",
                repo_dir.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "git reset --keep {target} refused in {}: {}",
            repo_dir.display(),
            git_output_text(&output)
        );
    }
    Ok(())
}

/// Where post-merge verification runs: the root checkout when it has trunk
/// checked out, otherwise the verify worktree at `post` (or the trunk tip).
fn post_merge_verification_dir(
    project_root: &std::path::Path,
    trunk_branch: &str,
    post: Option<&str>,
) -> Result<PathBuf> {
    if checks_out_branch(project_root, trunk_branch) {
        return Ok(project_root.to_path_buf());
    }
    let target = match post {
        Some(post) => post.to_string(),
        None => trunk_head(project_root, trunk_branch)?,
    };
    prepare_verify_worktree(project_root, &target)
}

/// Check out `target` (detached) in `.batty/verify-worktree`, reusing the
/// worktree (and its build cache) when possible and recreating it otherwise.
fn prepare_verify_worktree(project_root: &std::path::Path, target: &str) -> Result<PathBuf> {
    let path = project_root.join(VERIFY_WORKTREE_DIR);
    if path.join(".git").exists() {
        let checkout = crate::git::cmd(&path)
            .args(["checkout", "--detach", "--force", target])
            .allow_fail()
            .run();
        match checkout {
            Ok(output) if output.status.success() => {
                let clean = crate::git::cmd(&path)
                    .args(["clean", "-fdq"])
                    .allow_fail()
                    .run();
                if let Err(error) = clean {
                    warn!(error = %error, "failed to clean the verify worktree");
                }
                return Ok(path);
            }
            Ok(output) => warn!(
                stderr = %git_output_text(&output),
                "verify worktree checkout failed; recreating it"
            ),
            Err(error) => warn!(error = %error, "verify worktree checkout failed; recreating it"),
        }
        let path_arg = path.to_string_lossy().into_owned();
        let _ = crate::git::cmd(project_root)
            .args(["worktree", "remove", "--force", &path_arg])
            .allow_fail()
            .run();
    }
    if path.exists() {
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("failed to remove stale {}", path.display()))?;
    }
    let _ = crate::git::cmd(project_root)
        .args(["worktree", "prune"])
        .allow_fail()
        .run();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let path_arg = path.to_string_lossy().into_owned();
    let output = crate::git::cmd(project_root)
        .args(["worktree", "add", "--detach", &path_arg, target])
        .allow_fail()
        .run()
        .context("failed to run git worktree add for the verify worktree")?;
    if !output.status.success() {
        bail!(
            "git worktree add {} {target} failed: {}",
            path.display(),
            git_output_text(&output)
        );
    }
    Ok(path)
}

/// Run the post-merge test command in `dir`. Only the pass/fail result and
/// output matter here; the engineer-worktree checks of completion
/// verification do not apply to a trunk checkout.
fn run_post_merge_tests(
    dir: &std::path::Path,
    test_command: Option<&str>,
    child_pgid: &std::sync::atomic::AtomicU32,
) -> Result<VerificationRunResult> {
    let run = run_tests_in_worktree_tracked(dir, test_command, Some(child_pgid))?;
    Ok(VerificationRunResult {
        passed: run.passed,
        output: run.output,
        results: run.results,
        failures: Vec::new(),
        file_paths: Vec::new(),
        recovery: None,
    })
}

/// Land a chat-approved merge on the board: record the review approval
/// (which moves the task to done), falling back to the plain done move.
fn approve_chat_merge(
    daemon: &mut TeamDaemon,
    board_dir: &std::path::Path,
    board_dir_str: &str,
    request: &MergeRequest,
) -> bool {
    match crate::team::task_cmd::cmd_review_structured_with_attribution(
        board_dir,
        request.task_id,
        "approve",
        None,
        "telegram",
        crate::team::task_cmd::StatusTransitionAttribution::bridge("bridge.telegram.merge"),
    ) {
        Ok(()) => {
            persist_completed_profile(daemon, board_dir, request.task_id);
            true
        }
        Err(error) => {
            warn!(
                task_id = request.task_id,
                error = %error,
                "failed to record chat merge approval; moving task to done directly"
            );
            let manager_name = daemon.manager_name(&request.engineer);
            move_task_to_done(
                daemon,
                board_dir,
                board_dir_str,
                request,
                manager_name.as_deref(),
            )
        }
    }
}

fn move_task_to_done(
    daemon: &mut TeamDaemon,
    board_dir: &std::path::Path,
    board_dir_str: &str,
    request: &MergeRequest,
    manager_name: Option<&str>,
) -> bool {
    if crate::team::task_cmd::transition_task_with_attribution(
        board_dir,
        request.task_id,
        "done",
        crate::team::task_cmd::StatusTransitionAttribution::daemon("daemon.merge_queue"),
    )
    .is_ok()
    {
        persist_completed_profile(daemon, board_dir, request.task_id);
        return true;
    }

    if crate::team::task_cmd::transition_task_with_attribution(
        board_dir,
        request.task_id,
        "review",
        crate::team::task_cmd::StatusTransitionAttribution::daemon("daemon.merge_queue"),
    )
    .is_ok()
        && crate::team::task_cmd::cmd_review_with_attribution(
            board_dir,
            request.task_id,
            "approved",
            None,
            crate::team::task_cmd::StatusTransitionAttribution::daemon("daemon.merge_queue"),
        )
        .is_ok()
    {
        persist_completed_profile(daemon, board_dir, request.task_id);
        return true;
    }

    daemon.run_kanban_md_nonfatal(
        &[
            "move",
            &request.task_id.to_string(),
            "done",
            "--claim",
            &request.engineer,
            "--dir",
            board_dir_str,
        ],
        &format!("move task #{} to done", request.task_id),
        manager_name
            .into_iter()
            .chain(std::iter::once(request.engineer.as_str())),
    )
}

fn persist_completed_profile(daemon: &TeamDaemon, board_dir: &std::path::Path, task_id: u32) {
    let Ok(tasks) = crate::task::load_tasks_from_dir(&board_dir.join("tasks")) else {
        return;
    };
    let Some(task) = tasks.into_iter().find(|task| task.id == task_id) else {
        return;
    };
    if let Err(error) =
        crate::team::allocation::persist_completed_task_profile(daemon.project_root(), &task)
    {
        warn!(task_id, error = %error, "failed to persist completed task profile");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::board::write_workflow_metadata;
    use crate::team::standup::MemberState;
    use crate::team::task_loop::setup_engineer_worktree;
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{engineer_member, git_ok, init_git_repo, manager_member};
    use std::path::Path;

    fn request(task_id: u32) -> MergeRequest {
        MergeRequest {
            task_id,
            engineer: "eng-1".to_string(),
            branch: format!("eng-1/task-{task_id}"),
            worktree_dir: PathBuf::from("/tmp/worktree"),
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
            origin: MergeOrigin::AutoMerge,
        }
    }

    #[test]
    fn contains_task_covers_queued_and_in_flight_requests() {
        let mut queue = MergeQueue::default();
        assert!(!queue.contains_task(1));
        queue.enqueue(request(1));
        queue.mark_in_flight_for_test(request(2));
        assert!(queue.contains_task(1));
        assert!(queue.contains_task(2));
        assert!(!queue.contains_task(3));
    }

    #[test]
    fn merge_request_without_origin_deserializes_as_auto_merge() {
        let mut value = serde_json::to_value(request(4)).unwrap();
        value.as_object_mut().unwrap().remove("origin");
        let restored: MergeRequest = serde_json::from_value(value).unwrap();
        assert_eq!(restored.origin, MergeOrigin::AutoMerge);
    }

    fn finished_event(step: Option<MergeQueueStep>) -> MergeQueueEvent {
        match step {
            Some(MergeQueueStep::Finished(event)) => event,
            other => panic!("expected a finished step, got {other:?}"),
        }
    }

    fn done(outcome: MergeQueueOutcome) -> Result<MergeAttempt> {
        Ok(MergeAttempt::Done(outcome))
    }

    fn in_flight(task_id: u32, verify: PostMergeVerify) -> InFlightMerge {
        InFlightMerge::new(
            request(task_id),
            "pre".to_string(),
            Some("post".to_string()),
            MergeMode::DirectRoot,
            None,
            verify,
            None,
        )
    }

    #[test]
    fn process_next_runs_requests_in_fifo_order() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(41));
        queue.enqueue(request(42));

        let first = finished_event(queue.process_next(|request| {
            assert_eq!(request.task_id, 41);
            done(MergeQueueOutcome::Success)
        }));
        let second = finished_event(queue.process_next(|request| {
            assert_eq!(request.task_id, 42);
            done(MergeQueueOutcome::Conflict)
        }));

        assert_eq!(first.task_id, 41);
        assert_eq!(second.task_id, 42);
        assert_eq!(queue.queued_len(), 0);
        assert_eq!(queue.active_task_id(), None);
    }

    #[test]
    fn take_status_update_reports_queue_state_changes() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(41));

        let initial = queue.take_status_update().unwrap();
        assert!(initial.contains("[merge] queued: 1"));
        assert!(queue.take_status_update().is_none());

        queue.process_next(|_| done(MergeQueueOutcome::Success));
        let updated = queue.take_status_update().unwrap();
        assert!(updated.contains("last: #41 merged"));
    }

    #[test]
    fn processor_error_requeues_request_with_backoff() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(99));
        queue.enqueue(request(100));
        let now = Instant::now();

        let taken = queue.begin_next(now).unwrap();
        let step = queue.finish(taken, Err(anyhow!("merge execution failed")), now);
        match step {
            MergeQueueStep::RetryScheduled {
                request,
                attempt,
                delay,
                error,
            } => {
                assert_eq!(request.task_id, 99);
                assert_eq!(attempt, 1);
                assert_eq!(delay, MERGE_RETRY_BASE_DELAY);
                assert!(error.to_string().contains("merge execution failed"));
            }
            other => panic!("expected retry, got {other:?}"),
        }
        assert_eq!(queue.active_task_id(), None);
        assert_eq!(queue.queued_len(), 2, "failed request must not be lost");
        // Backoff holds the head of the queue (keeps merges in FIFO order).
        assert!(queue.begin_next(now).is_none());
        let later = now + MERGE_RETRY_BASE_DELAY;
        assert_eq!(queue.begin_next(later).unwrap().task_id, 99);
    }

    #[test]
    fn processor_errors_give_up_after_max_attempts() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(7));
        let mut now = Instant::now();
        for attempt in 1..MAX_MERGE_ATTEMPTS {
            let taken = queue.begin_next(now).unwrap();
            let step = queue.finish(taken, Err(anyhow!("boom")), now);
            assert!(
                matches!(step, MergeQueueStep::RetryScheduled { attempt: a, .. } if a == attempt)
            );
            now += MERGE_RETRY_MAX_DELAY;
        }
        let taken = queue.begin_next(now).unwrap();
        let step = queue.finish(taken, Err(anyhow!("boom")), now);
        assert!(matches!(
            step,
            MergeQueueStep::GaveUp { attempts, .. } if attempts == MAX_MERGE_ATTEMPTS
        ));
        assert_eq!(queue.queued_len(), 0);
        assert!(queue.snapshot().attempts.is_empty());
        assert!(queue.take_status_update().unwrap().contains("#7 failed"));
    }

    #[test]
    fn retry_delay_grows_and_is_capped() {
        assert_eq!(merge_retry_delay(1), Duration::from_secs(5));
        assert_eq!(merge_retry_delay(2), Duration::from_secs(10));
        assert_eq!(merge_retry_delay(3), Duration::from_secs(20));
        assert_eq!(merge_retry_delay(40), MERGE_RETRY_MAX_DELAY);
    }

    #[test]
    fn success_clears_retry_counter() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(5));
        let now = Instant::now();
        let taken = queue.begin_next(now).unwrap();
        queue.finish(taken, Err(anyhow!("transient")), now);
        assert_eq!(queue.snapshot().attempts.get(&5), Some(&1));
        let later = now + MERGE_RETRY_MAX_DELAY;
        let taken = queue.begin_next(later).unwrap();
        queue.finish(taken, done(MergeQueueOutcome::Success), later);
        assert!(queue.snapshot().attempts.is_empty());
    }

    #[test]
    fn lock_busy_defers_without_counting_an_attempt() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(11));
        let now = Instant::now();
        let taken = queue.begin_next(now).unwrap();
        let step = queue.finish(taken, Ok(MergeAttempt::LockBusy), now);
        assert!(matches!(step, MergeQueueStep::Deferred));
        assert_eq!(queue.queued_len(), 1);
        assert!(queue.snapshot().attempts.is_empty());
        assert!(queue.begin_next(now).is_none());
        assert!(queue.begin_next(now + MERGE_LOCK_BUSY_DELAY).is_some());
    }

    #[test]
    fn panicked_attempt_is_recovered_on_next_tick() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(21));
        queue.enqueue(request(22));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            queue.process_next(|_| panic!("processor exploded"));
        }));
        assert!(result.is_err());
        // The request is still accounted for while "active".
        assert_eq!(queue.snapshot().queue.len(), 2);

        let now = Instant::now();
        let step = queue.recover_interrupted_attempt(now).unwrap();
        assert!(matches!(
            step,
            MergeQueueStep::RetryScheduled { ref request, attempt: 1, .. } if request.task_id == 21
        ));
        assert_eq!(queue.active_task_id(), None);
        assert_eq!(queue.queued_len(), 2);
        assert!(queue.recover_interrupted_attempt(now).is_none());
    }

    #[test]
    fn in_flight_merge_blocks_queue_until_finished() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(31));
        queue.enqueue(request(32));
        let now = Instant::now();
        let taken = queue.begin_next(now).unwrap();
        let step = queue.finish(
            taken,
            Ok(MergeAttempt::Verifying(Box::new(in_flight(
                31,
                PostMergeVerify::Pending,
            )))),
            now,
        );
        assert!(matches!(step, MergeQueueStep::VerificationStarted(31)));
        assert!(queue.is_verifying());
        assert_eq!(queue.in_flight_task_id(), Some(31));
        assert!(
            queue.begin_next(now).is_none(),
            "queue must stay serialized"
        );
        assert!(queue.take_status_update().unwrap().contains("#31"));

        queue.take_in_flight();
        assert_eq!(queue.begin_next(now).unwrap().task_id, 32);
    }

    #[test]
    fn suspend_in_flight_kills_job_and_marks_pending() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let job = TestJob::spawn("suspend", move |_| {
            let _ = release_rx.recv();
            Err(anyhow!("cancelled"))
        });
        let mut queue = MergeQueue {
            in_flight: Some(in_flight(41, PostMergeVerify::Running(job))),
            ..Default::default()
        };
        assert_eq!(queue.suspend_in_flight(), Some(41));
        assert!(matches!(
            queue.in_flight.as_ref().unwrap().verify,
            PostMergeVerify::Pending
        ));
        drop(release_tx);
        assert_eq!(MergeQueue::default().suspend_in_flight(), None);
    }

    #[test]
    fn snapshot_and_restore_round_trip_queue_and_in_flight() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(1));
        queue.enqueue(request(2));
        let now = Instant::now();
        let taken = queue.begin_next(now).unwrap();
        queue.finish(taken, Err(anyhow!("transient")), now);
        queue.in_flight = Some(in_flight(
            9,
            PostMergeVerify::Failed("tests failed".to_string()),
        ));

        let json = serde_json::to_string(&queue.snapshot()).unwrap();
        let persisted: PersistedMergeQueue = serde_json::from_str(&json).unwrap();
        assert_eq!(
            persisted
                .queue
                .iter()
                .map(|r| r.task_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );

        let mut restored = MergeQueue::default();
        restored.enqueue(request(2)); // duplicate of a persisted request
        restored.enqueue(request(3));
        restored.restore(persisted);
        assert_eq!(
            restored
                .snapshot()
                .queue
                .iter()
                .map(|r| r.task_id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(restored.snapshot().attempts.get(&1), Some(&1));
        let merge = restored.in_flight.as_ref().unwrap();
        assert_eq!(merge.request.task_id, 9);
        assert_eq!(merge.pre_merge_head, "pre");
        assert!(matches!(&merge.verify, PostMergeVerify::Failed(out) if out == "tests failed"));
    }

    #[test]
    fn restored_running_verification_becomes_pending() {
        let persisted = PersistedMergeQueue {
            queue: Vec::new(),
            in_flight: Some(PersistedInFlightMerge {
                request: request(4),
                pre_merge_head: "abc".to_string(),
                post_merge_head: None,
                mode: MergeMode::IsolatedIntegration,
                merge_reason: Some("dirty root".to_string()),
                failure_output: None,
            }),
            attempts: HashMap::new(),
        };
        let mut queue = MergeQueue::default();
        queue.restore(persisted);
        let merge = queue.in_flight.as_ref().unwrap();
        assert!(matches!(merge.verify, PostMergeVerify::Pending));
        assert_eq!(merge.mode, MergeMode::IsolatedIntegration);
    }

    #[test]
    fn old_state_without_merge_queue_fields_deserializes() {
        let persisted: PersistedMergeQueue = serde_json::from_str("{}").unwrap();
        assert_eq!(persisted, PersistedMergeQueue::default());
    }

    #[test]
    fn plan_post_merge_revert_only_resets_untouched_merge_result() {
        assert_eq!(
            plan_post_merge_revert(Ok("post".to_string()), "pre", Some("post")),
            RevertPlan::Reset
        );
        assert_eq!(
            plan_post_merge_revert(Ok("pre".to_string()), "pre", Some("post")),
            RevertPlan::AlreadyReverted
        );
        assert!(matches!(
            plan_post_merge_revert(Ok("other".to_string()), "pre", Some("post")),
            RevertPlan::Manual(reason) if reason.contains("moved")
        ));
        assert!(matches!(
            plan_post_merge_revert(Ok("post".to_string()), "pre", None),
            RevertPlan::Manual(_)
        ));
        assert!(matches!(
            plan_post_merge_revert(Err(anyhow!("no git")), "pre", Some("post")),
            RevertPlan::Manual(_)
        ));
    }

    #[test]
    fn dirty_main_review_merge_block_detail_names_staged_source_paths() {
        let detail = dirty_main_review_merge_block_detail(&RootDirtyState {
            staged_paths: vec!["src/lib.rs".to_string()],
            unstaged_paths: Vec::new(),
            runtime_paths: Vec::new(),
            source_paths: vec!["src/lib.rs".to_string()],
        })
        .unwrap();

        assert!(detail.contains("Dirty source paths: src/lib.rs."));
        assert!(detail.contains("Staged: src/lib.rs."));
        assert!(detail.contains("Next action: commit, stash, or clean"));
    }

    #[test]
    fn dirty_main_review_merge_block_detail_names_unstaged_source_paths() {
        let detail = dirty_main_review_merge_block_detail(&RootDirtyState {
            staged_paths: Vec::new(),
            unstaged_paths: vec!["src/lib.rs".to_string()],
            runtime_paths: Vec::new(),
            source_paths: vec!["src/lib.rs".to_string()],
        })
        .unwrap();

        assert!(detail.contains("Dirty source paths: src/lib.rs."));
        assert!(detail.contains("Unstaged/untracked: src/lib.rs."));
        assert!(detail.contains("Next action: commit, stash, or clean"));
    }

    #[test]
    fn dirty_main_review_merge_block_detail_ignores_runtime_only_paths() {
        assert!(
            dirty_main_review_merge_block_detail(&RootDirtyState {
                staged_paths: Vec::new(),
                unstaged_paths: vec![".batty/team_config/board/activity.jsonl".to_string()],
                runtime_paths: vec![".batty/team_config/board/activity.jsonl".to_string()],
                source_paths: Vec::new(),
            })
            .is_none()
        );
    }

    fn write_task_file(project_root: &Path, id: u32, title: &str, status: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n\nTask description.\n"
            ),
        )
        .unwrap();
    }

    fn write_completion_metadata(
        project_root: &Path,
        id: u32,
        title: &str,
        branch: &str,
        worktree_dir: &Path,
        commit: &str,
    ) {
        let task_path = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join(format!("{id:03}-{title}.md"));
        write_workflow_metadata(
            &task_path,
            &WorkflowMetadata {
                branch: Some(branch.to_string()),
                worktree_path: Some(worktree_dir.to_string_lossy().into_owned()),
                commit: Some(commit.to_string()),
                changed_paths: vec!["note.txt".to_string()],
                tests_run: Some(true),
                tests_passed: Some(true),
                test_results: None,
                artifacts: Vec::new(),
                outcome: Some("verification_passed".to_string()),
                review_blockers: Vec::new(),
            },
        )
        .unwrap();
    }

    fn write_retry_required_metadata(project_root: &Path, request: &MergeRequest, title: &str) {
        let task_path = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join(format!("{:03}-{title}.md", request.task_id));
        write_workflow_metadata(
            &task_path,
            &WorkflowMetadata {
                branch: Some(request.branch.clone()),
                worktree_path: Some(request.worktree_dir.to_string_lossy().into_owned()),
                commit: Some("abc1234".to_string()),
                changed_paths: vec!["src/lib.rs".to_string()],
                tests_run: Some(true),
                tests_passed: Some(false),
                test_results: None,
                artifacts: Vec::new(),
                outcome: Some("verification_retry_required".to_string()),
                review_blockers: Vec::new(),
            },
        )
        .unwrap();
    }

    fn current_head(repo_dir: &Path) -> String {
        String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo_dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string()
    }

    fn git_commit_with_timestamp(repo_dir: &Path, message: &str, timestamp: &str) {
        let status = Command::new("git")
            .args(["commit", "-m", message])
            .env("GIT_AUTHOR_DATE", timestamp)
            .env("GIT_COMMITTER_DATE", timestamp)
            .current_dir(repo_dir)
            .status()
            .unwrap();
        assert!(status.success(), "git commit should succeed");
    }

    #[test]
    fn verification_retry_required_merge_queue_redispatches_review_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-retry-test");
        write_task_file(&repo, 42, "retry-required-task", "review");

        let retry_request = request(42);
        write_retry_required_metadata(&repo, &retry_request, "retry-required-task");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_member_state_for_test("eng-1", MemberState::Idle);
        daemon.enqueue_merge_request(retry_request);

        drain_merge_queue(&mut daemon);

        let task_path = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("042-retry-required-task.md");
        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "in-progress");
        assert_eq!(task.claimed_by.as_deref(), Some("eng-1"));
        assert_eq!(task.review_owner, None);
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );

        let messages =
            crate::team::inbox::pending_messages(&crate::team::inbox::inboxes_root(&repo), "eng-1")
                .unwrap();
        assert!(messages.iter().any(|message| {
            message.body.contains("requires another verification pass")
                && message.body.contains("verification_retry_required")
        }));
    }

    #[test]
    fn daemon_process_merge_queue_keeps_remaining_requests_on_error() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-error");
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        // Multi-repo merges are unsupported, so execute_queued_merge errors.
        daemon.is_multi_repo = true;
        daemon.enqueue_merge_request(request(41));
        daemon.enqueue_merge_request(request(42));

        daemon.process_merge_queue().unwrap();

        // The failed request is requeued (with backoff), not dropped.
        assert_eq!(daemon.merge.queue.queued_len(), 2);
        assert_eq!(daemon.merge.queue.active_task_id(), None);
        assert_eq!(daemon.merge.queue.snapshot().attempts.get(&41), Some(&1));
        // ...and the change was persisted immediately.
        let persisted = crate::team::daemon::state::load_daemon_state(&repo)
            .expect("merge queue change should persist daemon state");
        assert_eq!(persisted.merge_queue.queue.len(), 2);
        assert_eq!(persisted.merge_queue.attempts.get(&41), Some(&1));
    }

    /// Poll the merge queue until nothing is queued or in flight.
    fn drain_merge_queue(daemon: &mut TeamDaemon) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            daemon.process_merge_queue().unwrap();
            if daemon.merge.queue.queued_len() == 0 && !daemon.merge.queue.is_verifying() {
                return;
            }
            assert!(Instant::now() < deadline, "merge queue did not drain");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn enqueue_persists_request_and_restart_restores_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-persist");
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members.clone());
        daemon.enqueue_merge_request(request(77));

        let mut restarted = make_test_daemon(&repo, members);
        restarted.restore_merge_queue_state();
        assert_eq!(restarted.merge.queue.queued_len(), 1);
        assert_eq!(
            restarted.merge.queue.snapshot().queue[0],
            MergeRequest {
                queued_at: restarted.merge.queue.snapshot().queue[0].queued_at,
                ..request(77)
            }
        );
    }

    #[test]
    fn queue_waits_while_main_smoke_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-smoke-wait");
        let mut daemon = make_test_daemon(&repo, vec![manager_member("manager", None)]);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let job = TestJob::spawn("smoke", move |_| {
            let _ = release_rx.recv();
            Err(anyhow!("released"))
        });
        daemon.verification.background_tests.main_smoke =
            Some(super::super::background_tests::MainSmokeJob {
                command: "true".to_string(),
                head: "abc".to_string(),
                job,
            });
        daemon.enqueue_merge_request(request(5));
        daemon.process_merge_queue().unwrap();
        assert_eq!(
            daemon.merge.queue.queued_len(),
            1,
            "merge must wait for smoke"
        );
        assert!(daemon.merge.queue.snapshot().attempts.is_empty());
        drop(release_tx);
    }

    #[test]
    fn in_flight_verification_blocks_main_smoke_start() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-blocks-smoke");
        let mut daemon = make_test_daemon(&repo, vec![manager_member("manager", None)]);
        daemon.config.team_config.workflow_policy.main_smoke.enabled = true;
        daemon.config.team_config.workflow_policy.main_smoke.command = Some("true".to_string());
        daemon
            .config
            .team_config
            .workflow_policy
            .main_smoke
            .interval_secs = 0;
        daemon.merge.queue.in_flight = Some(InFlightMerge::new(
            request(6),
            "pre".to_string(),
            None,
            MergeMode::DirectRoot,
            None,
            PostMergeVerify::Failed("x".to_string()),
            None,
        ));
        daemon.maybe_run_main_smoke().unwrap();
        assert!(daemon.verification.background_tests.main_smoke.is_none());
    }

    #[test]
    fn daemon_process_merge_queue_merges_and_completes_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-test");
        write_task_file(&repo, 42, "merge-queue-task", "review");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);
        let activity_dir = repo.join(".batty").join("team_config").join("board");
        std::fs::create_dir_all(&activity_dir).unwrap();
        std::fs::write(
            activity_dir.join("activity.jsonl"),
            "{\"event\":\"tick\"}\n",
        )
        .unwrap();
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        let commit = current_head(&worktree_dir);
        write_completion_metadata(
            &repo,
            42,
            "merge-queue-task",
            &branch,
            &worktree_dir,
            &commit,
        );

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(MergeRequest {
            task_id: 42,
            engineer: "eng-1".to_string(),
            branch,
            worktree_dir: worktree_dir.clone(),
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
            origin: MergeOrigin::AutoMerge,
        });

        drain_merge_queue(&mut daemon);

        assert_eq!(
            crate::team::test_support::git_stdout(&repo, &["show", "main:note.txt"]),
            "queued merge"
        );
        assert!(
            std::fs::read_to_string(activity_dir.join("activity.jsonl"))
                .unwrap()
                .contains("{\"event\":\"tick\"}")
        );
        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-merge-queue-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "done");
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );
        let manager_messages = crate::team::inbox::pending_messages(
            &crate::team::inbox::inboxes_root(&repo),
            "manager",
        )
        .unwrap();
        assert!(
            manager_messages.iter().any(|message| {
                message.body.contains("Task #42 completed from merge queue")
                    && message.body.contains("Merge: success")
                    && !message
                        .body
                        .contains("Merge mode: isolated integration checkout")
            }),
            "manager should see a normal successful merge: {manager_messages:?}"
        );
        assert!(
            crate::team::checkpoint::read_preserved_lane_record(&repo, "main").is_some(),
            "runtime-only dirty main state should be snapshotted before merge"
        );
    }

    #[test]
    fn daemon_process_merge_queue_merges_into_configured_integration_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-integration-test");
        git_ok(&repo, &["branch", "develop"]);
        write_task_file(&repo, 43, "integration-branch-task", "review");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        crate::team::task_loop::setup_engineer_worktree_from_trunk(
            &repo,
            &worktree_dir,
            "eng-1",
            &team_config_dir,
            "develop",
        )
        .unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "integration merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "integration merge"]);
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        let commit = current_head(&worktree_dir);
        write_completion_metadata(
            &repo,
            43,
            "integration-branch-task",
            &branch,
            &worktree_dir,
            &commit,
        );
        let main_before = crate::team::test_support::git_stdout(&repo, &["rev-parse", "main"]);

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.config.team_config.trunk_branch = "develop".to_string();
        // Passes only in a checkout of develop that contains the merge.
        daemon.config.team_config.workflow_policy.test_command =
            Some("test -f note.txt".to_string());
        daemon.set_active_task_for_test("eng-1", 43);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(MergeRequest {
            task_id: 43,
            engineer: "eng-1".to_string(),
            branch,
            worktree_dir: worktree_dir.clone(),
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
            origin: MergeOrigin::AutoMerge,
        });

        drain_merge_queue(&mut daemon);

        assert_eq!(
            crate::team::test_support::git_stdout(&repo, &["show", "develop:note.txt"]),
            "integration merge"
        );
        assert!(
            !repo.join("note.txt").exists(),
            "the root checkout (on main) is not used for verification"
        );
        assert_eq!(
            crate::team::test_support::git_stdout(&repo, &["rev-parse", "main"]),
            main_before,
            "main must stay untouched when trunk_branch points at an integration branch"
        );
        let manager_messages = crate::team::inbox::pending_messages(
            &crate::team::inbox::inboxes_root(&repo),
            "manager",
        )
        .unwrap();
        assert!(
            manager_messages.iter().any(|message| message
                .body
                .contains("Task #43 completed from merge queue")
                && message.body.contains("Merge: success")),
            "manager should see a successful integration-branch merge: {manager_messages:?}"
        );
    }

    #[test]
    fn daemon_process_merge_queue_records_binary_refresh_after_source_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-refresh-test");
        write_task_file(&repo, 733, "daemon-refresh-task", "review");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::create_dir_all(worktree_dir.join("src")).unwrap();
        std::fs::write(
            worktree_dir.join("src").join("daemon_refresh.rs"),
            "pub fn daemon_refresh_after_merge() -> bool { true }\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "src/daemon_refresh.rs"]);
        git_commit_with_timestamp(
            &worktree_dir,
            "Task #733: trigger daemon refresh after merge",
            "@2000000000",
        );
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        let commit = current_head(&worktree_dir);
        write_completion_metadata(
            &repo,
            733,
            "daemon-refresh-task",
            &branch,
            &worktree_dir,
            &commit,
        );

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 733);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(MergeRequest {
            task_id: 733,
            engineer: "eng-1".to_string(),
            branch,
            worktree_dir,
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: false,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
            origin: MergeOrigin::AutoMerge,
        });

        drain_merge_queue(&mut daemon);

        let state = crate::team::daemon::health::binary_freshness::load_binary_refresh_state(&repo)
            .unwrap()
            .expect("post-merge source change should record pending binary refresh");
        assert_eq!(
            state.phase,
            crate::team::daemon::health::binary_freshness::DaemonBinaryRefreshPhase::Pending
        );
        assert!(state.commits_behind >= 1);
        assert_eq!(
            state.last_subject,
            "Task #733: trigger daemon refresh after merge"
        );
    }

    #[test]
    fn daemon_process_merge_queue_blocks_dirty_main_source_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-dirty-main-test");
        write_task_file(&repo, 42, "dirty-main-task", "review");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        let commit = current_head(&worktree_dir);
        write_completion_metadata(
            &repo,
            42,
            "dirty-main-task",
            &branch,
            &worktree_dir,
            &commit,
        );
        std::fs::write(repo.join("src").join("lib.rs"), "pub fn dirty_main() {}\n").unwrap();

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(MergeRequest {
            task_id: 42,
            engineer: "eng-1".to_string(),
            branch,
            worktree_dir: worktree_dir.clone(),
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
            origin: MergeOrigin::AutoMerge,
        });

        drain_merge_queue(&mut daemon);

        let show = Command::new("git")
            .args(["show", "main:note.txt"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(!show.status.success(), "review branch should not merge");
        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-dirty-main-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "review");
        assert!(
            task.blocked_on
                .as_deref()
                .is_some_and(|block| block.contains("src/lib.rs")
                    && block.contains("Next action: commit, stash, or clean"))
        );
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));

        let manager_messages = crate::team::inbox::pending_messages(
            &crate::team::inbox::inboxes_root(&repo),
            "manager",
        )
        .unwrap();
        assert!(
            manager_messages.iter().any(|message| {
                message.body.contains("dirty source edits")
                    && message.body.contains("src/lib.rs")
                    && message.body.contains("Leave the task in review")
            }),
            "manager should see dirty-main block: {manager_messages:?}"
        );
    }

    #[test]
    fn daemon_process_merge_queue_reverts_when_post_merge_verify_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-post-merge-verify-test");
        write_task_file(&repo, 42, "post-merge-verify-task", "review");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("trigger.txt"), "fail main verify\n").unwrap();
        git_ok(&worktree_dir, &["add", "trigger.txt"]);
        git_ok(
            &worktree_dir,
            &["commit", "-m", "trigger post-merge verify failure"],
        );
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        let commit = current_head(&worktree_dir);
        write_completion_metadata(
            &repo,
            42,
            "post-merge-verify-task",
            &branch,
            &worktree_dir,
            &commit,
        );

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.config.team_config.workflow_policy.test_command =
            Some("sh -c 'test ! -f trigger.txt'".to_string());
        let main_before = current_head(&repo);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(MergeRequest {
            task_id: 42,
            engineer: "eng-1".to_string(),
            branch,
            worktree_dir,
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
            origin: MergeOrigin::AutoMerge,
        });

        drain_merge_queue(&mut daemon);

        assert!(!repo.join("trigger.txt").exists());
        assert_eq!(
            crate::team::test_support::git_stdout(&repo, &["rev-parse", "main"]),
            main_before,
            "a clean root on trunk is reset to the pre-merge commit"
        );
        assert!(
            crate::team::test_support::git_stdout(
                &repo,
                &["status", "--porcelain", "--", ".", ":(exclude).batty"]
            )
            .is_empty(),
            "root index and files follow the reverted ref"
        );
        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-post-merge-verify-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "review");
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );
    }

    /// A review task with a committed branch, queued for a verified merge.
    fn queued_verified_merge(tmp: &tempfile::TempDir, test_command: &str) -> (PathBuf, TeamDaemon) {
        queued_verified_merge_of(tmp, test_command, "trigger.txt")
    }

    /// [`queued_verified_merge`] whose branch adds `path` (contents
    /// "merged").
    fn queued_verified_merge_of(
        tmp: &tempfile::TempDir,
        test_command: &str,
        path: &str,
    ) -> (PathBuf, TeamDaemon) {
        let repo = init_git_repo(tmp, "batty-post-merge-bg-test");
        write_task_file(&repo, 42, "bg-verify-task", "review");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        let file = worktree_dir.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "merged\n").unwrap();
        git_ok(&worktree_dir, &["add", path]);
        git_ok(&worktree_dir, &["commit", "-m", "background verify"]);
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        let commit = current_head(&worktree_dir);
        write_completion_metadata(&repo, 42, "bg-verify-task", &branch, &worktree_dir, &commit);

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.config.team_config.workflow_policy.test_command = Some(test_command.to_string());
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(MergeRequest {
            task_id: 42,
            engineer: "eng-1".to_string(),
            branch,
            worktree_dir,
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
            origin: MergeOrigin::AutoMerge,
        });
        (repo, daemon)
    }

    fn task_status(repo: &Path) -> String {
        crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-bg-verify-task.md"),
        )
        .unwrap()
        .status
    }

    #[test]
    fn post_merge_verification_runs_off_the_poll_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("release");
        let command = format!(
            "sh -c 'while [ ! -f \"{}\" ]; do sleep 0.05; done'",
            marker.display()
        );
        let (repo, mut daemon) = queued_verified_merge(&tmp, &command);

        let started = Instant::now();
        daemon.process_merge_queue().unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        // Merged, but not finalized while tests run. The merge lock stays
        // held so nothing else lands on trunk during verification.
        assert_eq!(
            crate::team::test_support::git_stdout(&repo, &["show", "main:trigger.txt"]),
            "merged"
        );
        assert!(daemon.merge.queue.is_verifying());
        assert!(crate::team::merge::merge_lock_is_held(&repo));
        assert!(MergeLock::try_acquire(&repo).unwrap().is_none());
        assert_eq!(task_status(&repo), "review");
        let persisted = crate::team::daemon::state::load_daemon_state(&repo).unwrap();
        assert_eq!(
            persisted
                .merge_queue
                .in_flight
                .as_ref()
                .map(|m| m.request.task_id),
            Some(42)
        );
        daemon.process_merge_queue().unwrap();
        assert!(daemon.merge.queue.is_verifying());

        std::fs::write(&marker, "").unwrap();
        drain_merge_queue(&mut daemon);
        assert_eq!(task_status(&repo), "done");
        assert_eq!(daemon.active_task_id("eng-1"), None);
        let persisted = crate::team::daemon::state::load_daemon_state(&repo).unwrap();
        assert!(persisted.merge_queue.in_flight.is_none());
        assert!(!crate::team::merge::merge_lock_is_held(&repo));
    }

    #[test]
    fn post_merge_verification_that_cannot_run_reverts_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, mut daemon) = queued_verified_merge(&tmp, "true");
        let pre = current_head(&repo);
        daemon.process_merge_queue().unwrap();
        assert!(daemon.merge.queue.is_verifying());
        // Replace the running job with one whose worker died without a result.
        let job: TestJob<VerificationRunResult> =
            TestJob::spawn("dies", |_| panic!("worker crashed"));
        daemon.merge.queue.in_flight.as_mut().unwrap().verify = PostMergeVerify::Running(job);

        drain_merge_queue(&mut daemon);
        assert_eq!(
            current_head(&repo),
            pre,
            "unverified merge must be reverted"
        );
        assert_eq!(task_status(&repo), "review");
    }

    #[test]
    fn failed_verification_does_not_reset_when_main_moved() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("release");
        let command = format!(
            "sh -c 'while [ ! -f \"{}\" ]; do sleep 0.05; done; exit 1'",
            marker.display()
        );
        let (repo, mut daemon) = queued_verified_merge(&tmp, &command);
        daemon.process_merge_queue().unwrap();
        assert!(daemon.merge.queue.is_verifying());
        // Someone else lands a commit on main while verification runs.
        std::fs::write(repo.join("other.txt"), "other\n").unwrap();
        git_ok(&repo, &["add", "other.txt"]);
        git_ok(&repo, &["commit", "-m", "concurrent landing"]);
        let moved = current_head(&repo);

        std::fs::write(&marker, "").unwrap();
        drain_merge_queue(&mut daemon);
        assert_eq!(current_head(&repo), moved, "must not discard later commits");
        let manager_messages = crate::team::inbox::pending_messages(
            &crate::team::inbox::inboxes_root(&repo),
            "manager",
        )
        .unwrap();
        assert!(
            manager_messages
                .iter()
                .any(|message| message.body.contains("could not revert it automatically")),
            "manager should be told to revert manually: {manager_messages:?}"
        );
    }

    #[test]
    fn interrupted_verification_resumes_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, mut daemon) = queued_verified_merge(&tmp, "sleep 30");
        daemon.process_merge_queue().unwrap();
        assert!(daemon.merge.queue.is_verifying());

        // Shutdown / hot reload path: kill tests, persist state.
        daemon.stop_background_tests();
        daemon.persist_runtime_state(false).unwrap();
        drop(daemon);

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut restarted = make_test_daemon(&repo, members);
        restarted.config.team_config.workflow_policy.test_command = Some("true".to_string());
        restarted.set_active_task_for_test("eng-1", 42);
        restarted.restore_runtime_state();
        assert_eq!(restarted.merge.queue.in_flight_task_id(), Some(42));
        assert!(matches!(
            restarted.merge.queue.in_flight.as_ref().unwrap().verify,
            PostMergeVerify::Pending
        ));

        drain_merge_queue(&mut restarted);
        assert_eq!(task_status(&repo), "done");
        assert_eq!(
            crate::team::test_support::git_stdout(&repo, &["show", "main:trigger.txt"]),
            "merged"
        );
    }

    fn manager_inbox(repo: &Path) -> Vec<String> {
        crate::team::inbox::pending_messages(&crate::team::inbox::inboxes_root(repo), "manager")
            .unwrap()
            .into_iter()
            .map(|message| message.body)
            .collect()
    }

    /// Poll the merge queue until `done` holds.
    fn poll_until(daemon: &mut TeamDaemon, what: &str, mut done: impl FnMut(&TeamDaemon) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            daemon.process_merge_queue().unwrap();
            if done(daemon) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A shell command that waits for `marker` to exist, then exits with
    /// `exit_code`.
    fn gated_command(marker: &Path, exit_code: i32) -> String {
        format!(
            "sh -c 'while [ ! -f \"{}\" ]; do sleep 0.05; done; exit {exit_code}'",
            marker.display()
        )
    }

    /// Review task #42 whose branch (cut from `develop`) adds trigger.txt,
    /// queued for a verified merge into trunk `develop` while the root
    /// checkout stays on `main`. Returns the repo, daemon and develop's
    /// pre-merge commit.
    fn queued_develop_merge(
        tmp: &tempfile::TempDir,
        test_command: &str,
    ) -> (PathBuf, TeamDaemon, String) {
        let repo = init_git_repo(tmp, "batty-develop-verify-test");
        git_ok(&repo, &["branch", "develop"]);
        write_task_file(&repo, 42, "bg-verify-task", "review");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        crate::team::task_loop::setup_engineer_worktree_from_trunk(
            &repo,
            &worktree_dir,
            "eng-1",
            &team_config_dir,
            "develop",
        )
        .unwrap();
        std::fs::write(worktree_dir.join("trigger.txt"), "merged\n").unwrap();
        git_ok(&worktree_dir, &["add", "trigger.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "develop work"]);
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        let commit = current_head(&worktree_dir);
        write_completion_metadata(&repo, 42, "bg-verify-task", &branch, &worktree_dir, &commit);
        let pre = crate::team::test_support::git_stdout(&repo, &["rev-parse", "develop"]);

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.config.team_config.trunk_branch = "develop".to_string();
        daemon.config.team_config.workflow_policy.test_command = Some(test_command.to_string());
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(MergeRequest {
            task_id: 42,
            engineer: "eng-1".to_string(),
            branch,
            worktree_dir,
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
            origin: MergeOrigin::AutoMerge,
        });
        (repo, daemon, pre)
    }

    #[test]
    fn failed_verification_reverts_configured_trunk_not_root_head() {
        let tmp = tempfile::tempdir().unwrap();
        // Fails only where the merge is checked out: the old code ran it in
        // the root (on main), where it passed.
        let (repo, mut daemon, pre) = queued_develop_merge(&tmp, "sh -c 'test ! -f trigger.txt'");
        let main_before = crate::team::test_support::git_stdout(&repo, &["rev-parse", "main"]);

        daemon.process_merge_queue().unwrap();
        assert!(daemon.merge.queue.is_verifying());
        let post = crate::team::test_support::git_stdout(&repo, &["rev-parse", "develop"]);
        assert_ne!(post, pre, "the merge landed on develop");
        assert_eq!(
            daemon
                .merge
                .queue
                .in_flight
                .as_ref()
                .unwrap()
                .pre_merge_head,
            pre
        );
        drain_merge_queue(&mut daemon);

        assert_eq!(
            crate::team::test_support::git_stdout(&repo, &["rev-parse", "develop"]),
            pre,
            "develop must be moved back to its pre-merge commit"
        );
        assert_eq!(
            crate::team::test_support::git_stdout(&repo, &["rev-parse", "main"]),
            main_before
        );
        assert_eq!(
            crate::team::test_support::git_stdout(&repo, &["symbolic-ref", "HEAD"]),
            "refs/heads/main",
            "the root checkout is left on its branch"
        );
        assert_eq!(task_status(&repo), "review");
        assert!(
            manager_inbox(&repo)
                .iter()
                .any(|body| body.contains("reverted develop")),
            "manager should be told develop was reverted"
        );
    }

    #[test]
    fn passing_verification_on_configured_trunk_keeps_merge() {
        let tmp = tempfile::tempdir().unwrap();
        // Passes only in a checkout of develop that contains the merge.
        let (repo, mut daemon, pre) = queued_develop_merge(&tmp, "test -f trigger.txt");

        drain_merge_queue(&mut daemon);

        let develop = crate::team::test_support::git_stdout(&repo, &["rev-parse", "develop"]);
        assert_ne!(develop, pre);
        assert_eq!(
            crate::team::test_support::git_stdout(&repo, &["show", "develop:trigger.txt"]),
            "merged"
        );
        assert_eq!(task_status(&repo), "done");
        assert!(repo.join(VERIFY_WORKTREE_DIR).join("trigger.txt").exists());
    }

    #[test]
    fn failed_verification_escalates_when_configured_trunk_moved() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("release");
        let (repo, mut daemon, pre) = queued_develop_merge(&tmp, &gated_command(&marker, 1));
        daemon.process_merge_queue().unwrap();
        assert!(daemon.merge.queue.is_verifying());
        // Another commit lands on develop while verification runs.
        let tree = crate::team::test_support::git_stdout(&repo, &["rev-parse", "develop^{tree}"]);
        let moved = crate::team::test_support::git_stdout(
            &repo,
            &[
                "commit-tree",
                &tree,
                "-p",
                "develop",
                "-m",
                "concurrent landing",
            ],
        );
        git_ok(&repo, &["update-ref", "refs/heads/develop", &moved]);

        std::fs::write(&marker, "").unwrap();
        drain_merge_queue(&mut daemon);

        let develop = crate::team::test_support::git_stdout(&repo, &["rev-parse", "develop"]);
        assert_eq!(develop, moved, "later commits must not be discarded");
        assert_ne!(develop, pre);
        let inbox = manager_inbox(&repo);
        assert!(
            inbox
                .iter()
                .any(|body| body.contains("could not revert it automatically")),
            "manager should be told to revert manually: {inbox:?}"
        );
        assert!(
            !inbox.iter().any(|body| body.contains("reverted develop")),
            "no reverted notice when the ref did not move: {inbox:?}"
        );
    }

    #[test]
    fn failed_verification_keeps_engineers_next_task() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("release");
        let (repo, mut daemon) = queued_verified_merge(&tmp, &gated_command(&marker, 1));
        daemon.process_merge_queue().unwrap();
        assert!(daemon.merge.queue.is_verifying());
        // The engineer moved on to task #99 while #42 was being verified.
        daemon.set_active_task_for_test("eng-1", 99);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        std::fs::write(&marker, "").unwrap();
        drain_merge_queue(&mut daemon);

        assert!(!repo.join("trigger.txt").exists(), "merge was reverted");
        assert_eq!(daemon.active_task_id("eng-1"), Some(99));
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );
    }

    #[test]
    fn retry_required_skip_does_not_replace_engineers_next_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-retry-moved-on");
        write_task_file(&repo, 42, "retry-required-task", "review");
        let retry_request = request(42);
        write_retry_required_metadata(&repo, &retry_request, "retry-required-task");
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 77);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(retry_request);

        drain_merge_queue(&mut daemon);

        assert_eq!(daemon.active_task_id("eng-1"), Some(77));
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );
        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-retry-required-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "review");
        let messages =
            crate::team::inbox::pending_messages(&crate::team::inbox::inboxes_root(&repo), "eng-1")
                .unwrap();
        assert!(
            !messages
                .iter()
                .any(|message| message.body.contains("requires another verification pass")),
            "a stale merge request must not re-dispatch the old task: {messages:?}"
        );
    }

    #[test]
    fn restored_in_flight_merge_holds_merge_lock_until_finished() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, mut daemon) = queued_verified_merge(&tmp, "sleep 30");
        daemon.process_merge_queue().unwrap();
        assert!(daemon.merge.queue.is_verifying());
        daemon.stop_background_tests();
        daemon.persist_runtime_state(false).unwrap();
        drop(daemon);
        // Between processes the lock is free (the holder exited).
        drop(crate::team::merge::acquire_eventually_for_test(&repo).expect("lock is free"));

        let marker = tmp.path().join("release");
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut restarted = make_test_daemon(&repo, members);
        restarted.config.team_config.workflow_policy.test_command = Some(gated_command(&marker, 0));
        restarted.set_active_task_for_test("eng-1", 42);
        restarted.restore_merge_queue_state();

        assert_eq!(restarted.merge.queue.in_flight_task_id(), Some(42));
        assert!(
            restarted
                .merge
                .queue
                .in_flight
                .as_ref()
                .unwrap()
                .lock
                .is_some(),
            "restored in-flight merge re-takes the merge lock"
        );
        assert!(MergeLock::try_acquire(&repo).unwrap().is_none());
        restarted.process_merge_queue().unwrap();
        assert!(restarted.merge.queue.is_verifying());
        assert!(MergeLock::try_acquire(&repo).unwrap().is_none());

        std::fs::write(&marker, "").unwrap();
        drain_merge_queue(&mut restarted);
        assert_eq!(task_status(&repo), "done");
        assert!(crate::team::merge::acquire_eventually_for_test(&repo).is_some());
    }

    #[test]
    fn dirty_root_blocks_revert_until_cleaned() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("release");
        let (repo, mut daemon) = queued_verified_merge(&tmp, &gated_command(&marker, 1));
        let pre = current_head(&repo);
        daemon.process_merge_queue().unwrap();
        assert!(daemon.merge.queue.is_verifying());
        let post = current_head(&repo);
        assert_ne!(post, pre);
        // The user starts editing the root checkout while tests run.
        std::fs::write(repo.join("src").join("lib.rs"), "pub fn user_edit() {}\n").unwrap();

        std::fs::write(&marker, "").unwrap();
        poll_until(&mut daemon, "revert escalation", |_| {
            manager_inbox(&repo)
                .iter()
                .any(|body| body.contains("has not reverted"))
        });
        daemon.process_merge_queue().unwrap();

        assert_eq!(current_head(&repo), post, "trunk is left alone");
        assert!(daemon.merge.queue.is_verifying(), "revert stays pending");
        assert_eq!(
            std::fs::read_to_string(repo.join("src").join("lib.rs")).unwrap(),
            "pub fn user_edit() {}\n",
            "uncommitted root changes are never discarded"
        );
        let escalations: Vec<String> = manager_inbox(&repo)
            .into_iter()
            .filter(|body| body.contains("has not reverted"))
            .collect();
        assert_eq!(escalations.len(), 1, "escalated once: {escalations:?}");
        assert!(escalations[0].contains("src/lib.rs"));
        assert!(escalations[0].contains(&pre));

        git_ok(&repo, &["checkout", "--", "src/lib.rs"]);
        drain_merge_queue(&mut daemon);
        assert_eq!(current_head(&repo), pre);
        assert!(!repo.join("trigger.txt").exists());
    }

    #[test]
    fn refused_reset_keep_is_escalated() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("release");
        // A tracked runtime path: local edits to it do not block the revert
        // up front, but `reset --keep` refuses to overwrite them.
        let (repo, mut daemon) =
            queued_verified_merge_of(&tmp, &gated_command(&marker, 1), "target/kept.txt");
        daemon.process_merge_queue().unwrap();
        assert!(daemon.merge.queue.is_verifying());
        let post = current_head(&repo);
        std::fs::write(repo.join("target").join("kept.txt"), "local edit\n").unwrap();

        std::fs::write(&marker, "").unwrap();
        drain_merge_queue(&mut daemon);

        assert_eq!(current_head(&repo), post);
        assert_eq!(
            std::fs::read_to_string(repo.join("target").join("kept.txt")).unwrap(),
            "local edit\n"
        );
        let inbox = manager_inbox(&repo);
        assert!(
            inbox
                .iter()
                .any(|body| body.contains("could not revert it automatically")
                    && body.contains("reset --keep")),
            "refused reset must be escalated: {inbox:?}"
        );
    }

    #[test]
    fn in_flight_merge_lock_errors_escalate_once_then_back_off() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-lock-errors");
        let head = current_head(&repo);
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.merge.queue.in_flight = Some(InFlightMerge::new(
            request(42),
            head.clone(),
            Some(head),
            MergeMode::DirectRoot,
            None,
            PostMergeVerify::Failed("boom".to_string()),
            None,
        ));
        // A directory where the lock file should be: every attempt errors.
        let lock_path = crate::team::merge::merge_lock_path(&repo);
        std::fs::create_dir_all(&lock_path).unwrap();
        let lock_escalations = |repo: &Path| {
            manager_inbox(repo)
                .into_iter()
                .filter(|body| body.contains("could not take the merge lock"))
                .count()
        };

        for attempt in 1..MERGE_LOCK_FAILURE_ESCALATE_AFTER {
            daemon.process_merge_queue().unwrap();
            assert_eq!(daemon.merge.queue.lock_failures, attempt);
            assert_eq!(lock_escalations(&repo), 0);
        }
        daemon.process_merge_queue().unwrap();
        assert_eq!(
            daemon.merge.queue.lock_failures,
            MERGE_LOCK_FAILURE_ESCALATE_AFTER
        );
        assert_eq!(lock_escalations(&repo), 1);
        // Backed off: further ticks do not retry (or escalate again).
        daemon.process_merge_queue().unwrap();
        daemon.process_merge_queue().unwrap();
        assert_eq!(
            daemon.merge.queue.lock_failures,
            MERGE_LOCK_FAILURE_ESCALATE_AFTER
        );
        assert_eq!(lock_escalations(&repo), 1);
        assert!(daemon.merge.queue.is_verifying());

        // Once the lock works again (and the back-off elapsed) it proceeds.
        std::fs::remove_dir(&lock_path).unwrap();
        daemon.merge.queue.lock_retry_at = Some(Instant::now());
        daemon.process_merge_queue().unwrap();
        assert_eq!(daemon.merge.queue.lock_failures, 0);
        assert!(!daemon.merge.queue.is_verifying());
    }

    fn dirty_root(paths: &[&str]) -> RootCheckoutDirty {
        RootCheckoutDirty {
            trunk_branch: "main".to_string(),
            paths: paths.iter().map(|path| path.to_string()).collect(),
            stderr: "error: Your local changes would be overwritten".to_string(),
        }
    }

    #[test]
    fn dirty_root_deferral_retries_and_escalates_on_third() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(9));
        let now = Instant::now();
        for deferral in 1..=4 {
            queue.not_before = None;
            let next = queue.begin_next(now).unwrap();
            let step = queue.finish(
                next,
                Ok(MergeAttempt::DirtyRootDeferred(dirty_root(&["src/lib.rs"]))),
                now,
            );
            match step {
                MergeQueueStep::DirtyRootDeferred {
                    deferrals,
                    escalate,
                    dirty,
                    ..
                } => {
                    assert_eq!(deferrals, deferral);
                    assert_eq!(escalate, deferral == 3, "deferral {deferral}");
                    assert_eq!(dirty.paths, vec!["src/lib.rs".to_string()]);
                }
                other => panic!("expected a dirty-root deferral, got {other:?}"),
            }
            assert_eq!(queue.queued_len(), 1, "deferred requests stay queued");
            assert!(queue.not_before.is_some(), "retried on the backoff");
        }
        assert!(
            queue.snapshot().attempts.is_empty(),
            "deferrals do not use up merge attempts"
        );
        queue.not_before = None;
        let next = queue.begin_next(now).unwrap();
        queue.finish(next, done(MergeQueueOutcome::Success), now);
        assert!(queue.dirty_root_deferrals.is_empty());
    }

    #[test]
    fn dirty_root_deferral_escalates_after_thirty_minutes() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(10));
        let start = Instant::now();
        let next = queue.begin_next(start).unwrap();
        let first = queue.finish(
            next,
            Ok(MergeAttempt::DirtyRootDeferred(dirty_root(&["a.txt"]))),
            start,
        );
        assert!(matches!(
            first,
            MergeQueueStep::DirtyRootDeferred {
                escalate: false,
                ..
            }
        ));
        let later = start + DIRTY_ROOT_ESCALATE_AFTER_AGE;
        let next = queue.begin_next(later).unwrap();
        let second = queue.finish(
            next,
            Ok(MergeAttempt::DirtyRootDeferred(dirty_root(&["a.txt"]))),
            later,
        );
        assert!(matches!(
            second,
            MergeQueueStep::DirtyRootDeferred {
                deferrals: 2,
                escalate: true,
                ..
            }
        ));
    }

    #[test]
    fn dirty_root_deferral_escalation_lists_paths_once() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-dirty-root-deferral");
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.merge.queue.enqueue(request(11));
        let now = Instant::now();
        for _ in 0..4 {
            daemon.merge.queue.not_before = None;
            let next = daemon.merge.queue.begin_next(now).unwrap();
            let step = daemon.merge.queue.finish(
                next,
                Ok(MergeAttempt::DirtyRootDeferred(dirty_root(&[
                    "src/lib.rs",
                    "notes.txt",
                ]))),
                now,
            );
            daemon.handle_merge_queue_step(step);
        }
        let escalations: Vec<String> = manager_inbox(&repo)
            .into_iter()
            .filter(|body| body.contains("cannot merge"))
            .collect();
        assert_eq!(escalations.len(), 1, "{escalations:?}");
        assert!(escalations[0].contains("src/lib.rs, notes.txt"));
        assert!(escalations[0].contains("Task #11"));
    }

    #[test]
    fn queue_defers_while_merge_lock_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, mut daemon) = queued_verified_merge(&tmp, "true");
        let held = MergeLock::try_acquire(&repo).unwrap().unwrap();
        let started = Instant::now();
        daemon.process_merge_queue().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not block on the lock"
        );
        assert_eq!(daemon.merge.queue.queued_len(), 1);
        assert!(daemon.merge.queue.snapshot().attempts.is_empty());
        drop(held);
        // Backoff elapses, then the merge goes through.
        daemon.merge.queue.not_before = None;
        drain_merge_queue(&mut daemon);
        assert_eq!(task_status(&repo), "done");
    }

    #[test]
    fn merge_request_skip_reason_requires_review_status() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-status-test");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);
        let commit = current_head(&worktree_dir);

        for status in ["todo", "blocked", "done"] {
            write_task_file(&repo, 42, "merge-queue-status-task", status);
            write_completion_metadata(
                &repo,
                42,
                "merge-queue-status-task",
                "eng-1/task-42",
                &worktree_dir,
                &commit,
            );
            let reason = merge_request_skip_reason(
                &repo,
                &MergeRequest {
                    task_id: 42,
                    engineer: "eng-1".to_string(),
                    branch: "eng-1/task-42".to_string(),
                    worktree_dir: worktree_dir.clone(),
                    queued_at: Instant::now(),
                    test_passed: true,
                    should_post_merge_verify: true,
                    test_duration_ms: 1,
                    confidence: 0.95,
                    files_changed: 1,
                    lines_changed: 1,
                    origin: MergeOrigin::AutoMerge,
                },
                "main",
            )
            .unwrap();

            assert_eq!(
                reason,
                Some((
                    AutoMergeSkipReason::WrongStatus,
                    format!("task status is '{}' instead of 'review'", status)
                ))
            );
        }
    }

    #[test]
    fn merge_request_skip_reason_requires_completion_packet_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-packet-test");
        write_task_file(&repo, 42, "merge-queue-packet-task", "review");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);

        let reason = merge_request_skip_reason(
            &repo,
            &MergeRequest {
                task_id: 42,
                engineer: "eng-1".to_string(),
                branch: "eng-1/task-42".to_string(),
                worktree_dir,
                queued_at: Instant::now(),
                test_passed: true,
                should_post_merge_verify: true,
                test_duration_ms: 1,
                confidence: 0.95,
                files_changed: 1,
                lines_changed: 1,
                origin: MergeOrigin::AutoMerge,
            },
            "main",
        )
        .unwrap();

        assert!(matches!(
            reason,
            Some((AutoMergeSkipReason::MissingPacket, detail))
                if detail.contains("branch marker missing")
                    && detail.contains("commit marker missing")
                    && detail.contains("worktree marker missing")
        ));
    }

    #[test]
    fn unavailable_branch_detail_counts_against_configured_trunk() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-mainline-test");
        git_ok(&repo, &["checkout", "-b", "mainline"]);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "eng-1/task-44",
                worktree_dir.to_str().unwrap(),
                "mainline",
            ],
        );
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);

        let detail = unavailable_branch_detail(
            &MergeRequest {
                task_id: 44,
                engineer: "eng-1".to_string(),
                branch: "eng-1/task-44".to_string(),
                worktree_dir,
                queued_at: Instant::now(),
                test_passed: true,
                should_post_merge_verify: true,
                test_duration_ms: 1,
                confidence: 0.95,
                files_changed: 1,
                lines_changed: 1,
                origin: MergeOrigin::AutoMerge,
            },
            "mainline",
        )
        .unwrap();

        assert_eq!(detail, None);
    }

    #[test]
    fn daemon_process_merge_queue_skips_todo_task_even_with_branch_and_packet() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-queue-todo-skip-test");
        write_task_file(&repo, 42, "merge-queue-todo-task", "todo");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("note.txt"), "queued merge\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "queue merge"]);
        let commit = current_head(&worktree_dir);
        write_completion_metadata(
            &repo,
            42,
            "merge-queue-todo-task",
            "eng-1/task-42",
            &worktree_dir,
            &commit,
        );

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.enqueue_merge_request(MergeRequest {
            task_id: 42,
            engineer: "eng-1".to_string(),
            branch: "eng-1/task-42".to_string(),
            worktree_dir: worktree_dir.clone(),
            queued_at: Instant::now(),
            test_passed: true,
            should_post_merge_verify: true,
            test_duration_ms: 1,
            confidence: 0.95,
            files_changed: 1,
            lines_changed: 1,
            origin: MergeOrigin::AutoMerge,
        });

        drain_merge_queue(&mut daemon);

        assert!(!repo.join("note.txt").exists());
        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-merge-queue-todo-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "todo");
    }

    #[test]
    fn process_merge_queue_empty_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_test_daemon(tmp.path(), vec![]);
        drain_merge_queue(&mut daemon);
        assert_eq!(daemon.merge.queue.queued_len(), 0);
    }

    #[test]
    fn process_next_returns_event_with_outcome() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(99));

        let event = finished_event(queue.process_next(|_| done(MergeQueueOutcome::Success)));
        assert_eq!(event.task_id, 99);
        assert_eq!(event.engineer, "eng-1");
        assert!(matches!(event.outcome, MergeQueueOutcome::Success));
    }

    #[test]
    fn process_next_returns_conflict_outcome() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(88));

        let event = finished_event(queue.process_next(|_| done(MergeQueueOutcome::Conflict)));
        assert_eq!(event.task_id, 88);
        assert!(matches!(event.outcome, MergeQueueOutcome::Conflict));
    }
}
