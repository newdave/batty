//! Dispatch bookkeeping: the pending dispatch queue plus the dedupe,
//! rescue, release-exclusion and manual-assignment cooldown records that gate
//! which engineer may receive which task.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::dispatch::DispatchQueueEntry;
use super::{ReleaseRecord, RescueRecord};

#[derive(Default)]
pub(in crate::team) struct DispatchState {
    pub(in crate::team) queue: Vec<DispatchQueueEntry>,
    /// Tracks recent (task_id, engineer) dispatch pairs for deduplication.
    pub(in crate::team) recent_dispatches: HashMap<(u32, String), Instant>,
    /// #684 / #686: tasks the auto/runtime orphan rescue just moved back
    /// to todo. Dispatch skips these for an exponentially-growing cooldown
    /// window derived from `orphan_rescue_cooldown_secs` × 2^(count-1)
    /// (capped at 16×) so repeated rescues of the same task widen the
    /// quiet period instead of re-cascading every base window.
    pub(in crate::team) recently_rescued_tasks: HashMap<u32, RescueRecord>,
    /// #697: tasks recently released by a specific engineer. Populated
    /// when state reconciliation detects an engineer cleared their claim
    /// (reason `task no longer claimed by this engineer`). Dispatch
    /// excludes that engineer from re-dispatch of the same task for
    /// `dispatch_release_exclusion_secs` so they do not immediately
    /// receive back a task they just parked. Other engineers remain
    /// eligible once the task-level rescue cooldown expires.
    pub(in crate::team) recently_released_by: HashMap<(u32, String), ReleaseRecord>,
    /// Timestamp of the last manual assignment per engineer (for cooldown).
    pub(in crate::team) manual_assign_cooldowns: HashMap<String, Instant>,
}

impl DispatchState {
    /// #684 / #686: task IDs currently within the orphan-rescue cooldown
    /// window (exponentially grown per repeated rescue from `base`).
    /// Dispatch filters these out so a task the releasing engineer parked
    /// doesn't immediately bounce to a peer, and tasks that keep getting
    /// rescued stay quiet longer instead of re-cascading every base window.
    pub(in crate::team) fn rescued_task_ids(&self, base: Duration) -> HashSet<u32> {
        self.recently_rescued_tasks
            .iter()
            .filter(|(_, record)| record.dispatch_blocked(base))
            .map(|(task_id, _)| *task_id)
            .collect()
    }

    /// #686 / #689: record a rescue event for `task_id`. Growth condition
    /// uses the cascade-observation window (2× effective cooldown), not
    /// the dispatch-gate window — rescues can only fire *after* the
    /// dispatch gate has opened, so gating growth on `dispatch_blocked`
    /// meant the counter never climbed past 1 in production.
    pub(in crate::team) fn record_task_rescue(&mut self, task_id: u32, base: Duration) {
        let now = Instant::now();
        self.recently_rescued_tasks
            .entry(task_id)
            .and_modify(|record| {
                if record.in_cascade_window(base) {
                    record.count = record.count.saturating_add(1);
                } else {
                    record.count = 1;
                }
                record.last_rescued_at = now;
            })
            .or_insert(RescueRecord {
                last_rescued_at: now,
                count: 1,
            });
    }

    /// #697 / #698: record that `engineer` just released task `task_id`
    /// (state reconciliation observed `claimed_by` cleared). Subsequent
    /// dispatch cycles skip this pair until the exclusion window
    /// elapses. #698: if the previous release for this pair is still
    /// inside the cascade-observation window (2× effective window), the
    /// counter grows — widening the next exclusion window exponentially
    /// up to 16× base. This prevents a human-parked task (owner
    /// awaiting a Saturday ping, for instance) from cycling through
    /// dispatch→release every base window for ~14 hours.
    pub(in crate::team) fn record_task_release_by(
        &mut self,
        task_id: u32,
        engineer: &str,
        base: Duration,
    ) {
        let now = Instant::now();
        self.recently_released_by
            .entry((task_id, engineer.to_string()))
            .and_modify(|record| {
                if record.in_cascade_window(base) {
                    record.count = record.count.saturating_add(1);
                } else {
                    record.count = 1;
                }
                record.last_released_at = now;
            })
            .or_insert(ReleaseRecord {
                last_released_at: now,
                count: 1,
            });
    }

    /// #697 / #698: true if `engineer` is still within the current
    /// (exponentially-grown from `base`) release-exclusion window for `task_id`.
    pub(in crate::team) fn is_release_excluded(
        &self,
        task_id: u32,
        engineer: &str,
        base: Duration,
    ) -> bool {
        self.recently_released_by
            .get(&(task_id, engineer.to_string()))
            .map(|record| record.dispatch_excluded(base))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_exclusion_is_per_task_and_engineer() {
        let mut state = DispatchState::default();
        let base = Duration::from_secs(60);
        assert!(!state.is_release_excluded(1, "eng-1", base));
        state.record_task_release_by(1, "eng-1", base);
        assert!(state.is_release_excluded(1, "eng-1", base));
        assert!(!state.is_release_excluded(1, "eng-2", base));
        assert!(!state.is_release_excluded(2, "eng-1", base));
        assert!(!state.is_release_excluded(1, "eng-1", Duration::ZERO));
    }

    #[test]
    fn repeated_rescue_within_cascade_window_grows_count() {
        let mut state = DispatchState::default();
        let base = Duration::from_secs(60);
        state.record_task_rescue(7, base);
        state.record_task_rescue(7, base);
        assert_eq!(state.recently_rescued_tasks[&7].count, 2);
        assert_eq!(state.rescued_task_ids(base), HashSet::from([7]));
        assert!(state.rescued_task_ids(Duration::ZERO).is_empty());
    }
}
