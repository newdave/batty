//! Per-daemon state for work that must not block the poll tick: scheduled
//! retries, pending member removals, and repeated-failure log throttling.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Largest gap, in poll cycles, between two reports of the same failure.
const MAX_REPORT_GAP_CYCLES: u32 = 64;

/// How often a throttled step reports how many failures it swallowed.
pub(super) const FAILURE_SUMMARY_INTERVAL: Duration = Duration::from_secs(600);

/// Failures swallowed by the throttle since the last summary for one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FailureSummary {
    pub(super) step: String,
    pub(super) count_since_last: u32,
    pub(super) last_error: String,
}

/// A retry notice scheduled for a member after a stalled-mid-turn completion.
#[derive(Debug, Clone)]
pub(super) struct ScheduledStallRetry {
    pub(super) retry_at: Instant,
    pub(super) backoff_secs: u64,
    pub(super) response: String,
}

/// A member whose shim was asked to shut down and is reaped on later ticks.
#[derive(Debug, Clone)]
pub(super) struct PendingRemoval {
    pub(super) pid: u32,
    pub(super) deadline: Instant,
    pub(super) was_working: bool,
    /// SIGKILL already sent after the deadline.
    pub(super) killed: bool,
}

#[derive(Debug, Clone)]
struct FailureThrottle {
    last_error: String,
    /// Failures swallowed since the last report.
    skipped: u32,
    /// Failures to swallow before the next report.
    gap: u32,
    /// Failures swallowed (never reported individually) since the last summary.
    unreported: u32,
    /// When the current summary window began.
    window_started: Instant,
}

/// Decides when a repeating failure is worth reporting: the first time, when
/// the error text changes, then with exponential backoff (1, 2, 4 .. 64
/// cycles between reports). Cleared when the step succeeds.
#[derive(Debug, Default)]
pub(super) struct FailureThrottles {
    entries: HashMap<String, FailureThrottle>,
}

impl FailureThrottles {
    pub(super) fn should_report(&mut self, key: &str, error: &str) -> bool {
        let Some(entry) = self.entries.get_mut(key) else {
            self.entries.insert(
                key.to_string(),
                FailureThrottle {
                    last_error: error.to_string(),
                    skipped: 0,
                    gap: 1,
                    unreported: 0,
                    window_started: Instant::now(),
                },
            );
            return true;
        };
        if entry.last_error != error {
            entry.last_error = error.to_string();
            entry.skipped = 0;
            entry.gap = 1;
            return true;
        }
        entry.skipped += 1;
        if entry.skipped >= entry.gap {
            entry.skipped = 0;
            entry.gap = (entry.gap * 2).min(MAX_REPORT_GAP_CYCLES);
            return true;
        }
        false
    }

    /// Record a failure that `should_report` throttled and that was not
    /// emitted by any other path.
    pub(super) fn note_suppressed(&mut self, key: &str) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.unreported += 1;
        }
    }

    /// Summaries for every step with swallowed failures whose window is at
    /// least `interval` old as of `now`. Each returned step starts a new window.
    pub(super) fn take_due_summaries(
        &mut self,
        now: Instant,
        interval: Duration,
    ) -> Vec<FailureSummary> {
        let mut due = Vec::new();
        for (step, entry) in &mut self.entries {
            if now.saturating_duration_since(entry.window_started) < interval {
                continue;
            }
            entry.window_started = now;
            if entry.unreported > 0 {
                due.push(FailureSummary {
                    step: step.clone(),
                    count_since_last: std::mem::take(&mut entry.unreported),
                    last_error: entry.last_error.clone(),
                });
            }
        }
        due.sort_by(|a, b| a.step.cmp(&b.step));
        due
    }

    /// Forget `key` after a success, returning any swallowed failures that
    /// were never summarised so the caller can still account for them.
    pub(super) fn clear(&mut self, key: &str) -> Option<FailureSummary> {
        let entry = self.entries.remove(key)?;
        (entry.unreported > 0).then(|| FailureSummary {
            step: key.to_string(),
            count_since_last: entry.unreported,
            last_error: entry.last_error,
        })
    }
}

#[derive(Debug, Default)]
pub(in crate::team) struct DeferredState {
    pub(super) stall_retries: HashMap<String, ScheduledStallRetry>,
    pub(super) pending_removals: HashMap<String, PendingRemoval>,
    pub(super) step_failures: FailureThrottles,
    /// Last warning text emitted per dedupe key; a warning is logged again
    /// only when its text changes.
    pub(super) last_warnings: HashMap<String, String>,
    /// Binary fingerprints for which a hot-reload warning was already logged.
    pub(super) hot_reload_warned: HashSet<String>,
    /// Last time each engineer's worktree was inspected for uncommitted work.
    pub(super) last_uncommitted_inspection: HashMap<String, Instant>,
}

impl DeferredState {
    /// Returns true when `message` differs from the last one logged for `key`.
    pub(super) fn warn_changed(&mut self, key: &str, message: &str) -> bool {
        if self.last_warnings.get(key).is_some_and(|m| m == message) {
            return false;
        }
        self.last_warnings
            .insert(key.to_string(), message.to_string());
        true
    }

    pub(super) fn clear_warning(&mut self, key: &str) {
        self.last_warnings.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_report_cycles_back_off_exponentially() {
        let mut t = FailureThrottles::default();
        let cycles: Vec<u32> = (1..=40u32).filter(|_| t.should_report("s", "e")).collect();
        assert_eq!(cycles.len(), 6);
        let mut t = FailureThrottles::default();
        let mut cycles = Vec::new();
        for cycle in 1..=40u32 {
            if t.should_report("s", "e") {
                cycles.push(cycle);
            }
        }
        assert_eq!(cycles, vec![1, 2, 4, 8, 16, 32]);
    }

    #[test]
    fn throttle_reports_when_error_changes_and_resets_on_clear() {
        let mut t = FailureThrottles::default();
        assert!(t.should_report("s", "a"));
        assert!(t.should_report("s", "a"));
        assert!(!t.should_report("s", "a"));
        assert!(t.should_report("s", "b"));
        t.clear("s");
        assert!(t.should_report("s", "b"));
    }

    #[test]
    fn summaries_count_only_suppressed_failures_per_window() {
        let mut t = FailureThrottles::default();
        let start = Instant::now();
        for _ in 0..10 {
            if !t.should_report("s", "boom") {
                t.note_suppressed("s");
            }
        }
        // Reported at cycles 1, 2, 4, 8 -> 6 swallowed.
        assert!(
            t.take_due_summaries(start, FAILURE_SUMMARY_INTERVAL)
                .is_empty()
        );
        let later = start + FAILURE_SUMMARY_INTERVAL + Duration::from_secs(1);
        assert_eq!(
            t.take_due_summaries(later, FAILURE_SUMMARY_INTERVAL),
            vec![FailureSummary {
                step: "s".into(),
                count_since_last: 6,
                last_error: "boom".into(),
            }]
        );
        // Window restarted and counter drained.
        assert!(
            t.take_due_summaries(later + FAILURE_SUMMARY_INTERVAL, FAILURE_SUMMARY_INTERVAL)
                .is_empty()
        );
    }

    #[test]
    fn clear_returns_unsummarised_failures() {
        let mut t = FailureThrottles::default();
        for _ in 0..3 {
            if !t.should_report("s", "x") {
                t.note_suppressed("s");
            }
        }
        let summary = t.clear("s").expect("one swallowed failure");
        assert_eq!(summary.count_since_last, 1);
        assert!(t.clear("s").is_none());
    }

    #[test]
    fn throttle_gap_is_capped() {
        let mut t = FailureThrottles::default();
        let reports = (1..=1000).filter(|_| t.should_report("s", "e")).count();
        // 1,2,4,8,16,32 then every 64 cycles.
        assert!(reports > 6 && reports < 30, "reports = {reports}");
    }

    #[test]
    fn warn_changed_dedupes_identical_messages() {
        let mut d = DeferredState::default();
        assert!(d.warn_changed("k", "m1"));
        assert!(!d.warn_changed("k", "m1"));
        assert!(d.warn_changed("k", "m2"));
        d.clear_warning("k");
        assert!(d.warn_changed("k", "m2"));
    }
}
