//! Per-daemon state for work that must not block the poll tick: scheduled
//! retries, pending member removals, and repeated-failure log throttling.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Largest gap, in poll cycles, between two reports of the same failure.
const MAX_REPORT_GAP_CYCLES: u32 = 64;

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

    pub(super) fn clear(&mut self, key: &str) {
        self.entries.remove(key);
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
