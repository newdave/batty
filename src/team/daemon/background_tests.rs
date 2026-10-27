//! Daemon bookkeeping for test commands that run off the poll loop.
//!
//! Completion verification, the main smoke check and chat `merge` commands
//! run their test command on a background thread (`team::test_job`). The
//! poll loop checks for finished jobs every tick and runs the continuation
//! against the then-current state.

use std::collections::HashMap;

use anyhow::Result;
use tracing::{info, warn};

use super::TeamDaemon;
use crate::team::merge::{self, VerificationJob};
use crate::team::test_job::{RunningTestInfo, TestJob};
use crate::team::test_results::TestRunOutput;

#[derive(Default)]
pub(crate) struct BackgroundTests {
    /// Completion verifications in flight, keyed by engineer.
    pub(crate) verification: HashMap<String, VerificationJob>,
    pub(super) main_smoke: Option<MainSmokeJob>,
    pub(super) chat_merges: Vec<ChatMergeJob>,
    /// Completion verifications (engineer -> task) interrupted by a daemon
    /// restart or hot reload; re-run on the next poll.
    pub(crate) retriggers: HashMap<String, u32>,
}

pub(super) struct MainSmokeJob {
    pub(super) command: String,
    pub(super) head: String,
    pub(super) job: TestJob<TestRunOutput>,
}

/// Where to send the reply of a chat command that finishes in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChatReplyTarget {
    /// Only log the reply (direct callers such as tests).
    #[cfg_attr(not(test), allow(dead_code))]
    None,
    Telegram {
        chat_id: i64,
    },
    Discord,
}

pub(super) struct ChatMergeJob {
    pub(super) task_id: u32,
    pub(super) engineer: String,
    pub(super) reply_to: ChatReplyTarget,
    pub(super) job: TestJob<TestRunOutput>,
}

impl BackgroundTests {
    /// True while the main smoke check runs on the root checkout; nothing
    /// may merge into trunk underneath it.
    pub(crate) fn main_smoke_running(&self) -> bool {
        self.main_smoke.is_some()
    }

    /// Jobs currently running, for `batty status`.
    pub(crate) fn running_tests(&self) -> Vec<RunningTestInfo> {
        let mut running: Vec<RunningTestInfo> = self
            .verification
            .values()
            .map(VerificationJob::running_info)
            .collect();
        running.extend(self.main_smoke.iter().map(|smoke| smoke.job.info()));
        running.extend(self.chat_merges.iter().map(|merge| merge.job.info()));
        running.sort_by(|a, b| a.label.cmp(&b.label));
        running
    }

    /// Verifications that must survive a restart: those in flight plus those
    /// still waiting to be re-run.
    pub(crate) fn pending_verifications(&self) -> HashMap<String, u32> {
        let mut pending = self.retriggers.clone();
        for (engineer, job) in &self.verification {
            pending.insert(engineer.clone(), job.task_id());
        }
        pending
    }
}

impl TeamDaemon {
    /// Wall-clock limit for background test commands, from
    /// `workflow_policy.verification.timeout_secs` (`None` when 0).
    pub(crate) fn test_job_timeout(&self) -> Option<std::time::Duration> {
        match self
            .config
            .team_config
            .workflow_policy
            .verification
            .timeout_secs
        {
            0 => None,
            secs => Some(std::time::Duration::from_secs(secs)),
        }
    }

    /// Poll-loop step: re-run interrupted verifications and apply results of
    /// finished background test jobs.
    pub(crate) fn poll_background_tests(&mut self) -> Result<()> {
        self.retrigger_pending_verifications();
        let result = merge::poll_verification_jobs(self);
        self.poll_chat_merge_jobs();
        result
    }

    fn retrigger_pending_verifications(&mut self) {
        if self.verification.background_tests.retriggers.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.verification.background_tests.retriggers);
        for (engineer, task_id) in pending {
            if self.active_task_id(&engineer) != Some(task_id) {
                info!(
                    engineer = %engineer,
                    task_id,
                    "dropping interrupted verification: task is no longer active for this engineer"
                );
                continue;
            }
            info!(
                engineer = %engineer,
                task_id,
                "re-running completion verification interrupted by daemon restart"
            );
            if let Err(error) = merge::handle_engineer_completion(self, &engineer) {
                warn!(
                    engineer = %engineer,
                    task_id,
                    error = %error,
                    "failed to re-run interrupted completion verification"
                );
            }
        }
    }

    /// After a restart, keep ownership of tasks whose verification was
    /// interrupted, so the re-run is not mistaken for orphaned work. Entries
    /// whose task changed on the board are dropped.
    pub(super) fn restore_pending_verification_tasks(&mut self) {
        if self.verification.background_tests.retriggers.is_empty() {
            return;
        }
        let tasks_dir = self.board_dir().join("tasks");
        let tasks = match crate::task::load_tasks_from_dir(&tasks_dir) {
            Ok(tasks) => tasks,
            Err(error) => {
                warn!(error = %error, "failed to load board for interrupted verifications");
                return;
            }
        };
        let pending = std::mem::take(&mut self.verification.background_tests.retriggers);
        for (engineer, task_id) in pending {
            let still_owned = tasks.iter().any(|task| {
                task.id == task_id
                    && task.claimed_by.as_deref() == Some(engineer.as_str())
                    && matches!(task.status.as_str(), "in-progress" | "review")
            });
            if !still_owned {
                info!(
                    engineer = %engineer,
                    task_id,
                    "dropping interrupted verification: task changed on the board"
                );
                continue;
            }
            self.active_tasks.insert(engineer.clone(), task_id);
            self.verification
                .background_tests
                .retriggers
                .insert(engineer, task_id);
        }
    }

    /// Kill every background test command (shutdown, hot reload). In-flight
    /// completion verifications are kept as re-triggers, so they are
    /// persisted and re-run instead of being lost.
    pub(crate) fn stop_background_tests(&mut self) {
        for (engineer, job) in self.verification.background_tests.verification.drain() {
            info!(
                engineer = %engineer,
                task_id = job.task_id(),
                "stopping background verification; it will re-run after restart"
            );
            job.kill();
            self.verification
                .background_tests
                .retriggers
                .insert(engineer, job.task_id());
        }
        // A merge that already landed keeps its in-flight record (persisted
        // by the caller); its verification re-runs after restart.
        if let Some(task_id) = self.merge.queue.suspend_in_flight() {
            info!(
                task_id,
                "stopping post-merge verification; it will re-run after restart"
            );
        }
        if let Some(smoke) = self.verification.background_tests.main_smoke.take() {
            info!(command = %smoke.command, "stopping background main smoke check");
            smoke.job.kill();
        }
        for chat in std::mem::take(&mut self.verification.background_tests.chat_merges) {
            chat.job.kill();
            self.integrations.send_chat_reply(
                &chat.reply_to,
                &format!(
                    "Merge of Task #{} was interrupted by a daemon restart; send the merge command again.",
                    chat.task_id
                ),
            );
        }
    }
}
