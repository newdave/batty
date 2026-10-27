//! Cadence state for periodic daemon automation: standup timers, board
//! maintenance, planning cycles, telemetry housekeeping, and the keyed
//! cooldown maps that rate-limit repeated alerts.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub(in crate::team) struct Scheduler {
    pub(in crate::team) paused_standups: HashSet<String>,
    pub(in crate::team) last_standup: HashMap<String, Instant>,
    pub(in crate::team) last_board_rotation: Instant,
    pub(in crate::team) last_auto_archive: Instant,
    pub(in crate::team) last_auto_dispatch: Instant,
    pub(in crate::team) pipeline_starvation_fired: bool,
    pub(in crate::team) pipeline_starvation_last_fired: Option<Instant>,
    pub(in crate::team) planning_cycle_last_fired: Option<Instant>,
    pub(in crate::team) planning_cycle_active: bool,
    /// #681: Consecutive planning cycles that produced zero new tasks.
    /// Used to exponentially back off the architect planning cadence so
    /// that a stuck board (everything blocked, no dispatchable work) does
    /// not burn orchestrator tokens with a ping storm of empty cycles.
    pub(in crate::team) planning_cycle_consecutive_empty: u32,
    pub(in crate::team) retro_generated: bool,
    /// When the last binary freshness check ran (#675). Gated to at most once per hour.
    pub(in crate::team) last_binary_freshness_check: Option<Instant>,
    /// Last daily telemetry retention pass.
    pub(in crate::team) last_telemetry_retention: Instant,
    /// When the last tiered inbox expiry sweep ran (#658). Gated to at most once per minute.
    pub(in crate::team) last_tiered_inbox_sweep: Option<Instant>,
    /// Keyed cooldowns for interventions, alerts and optional-subsystem budgets.
    pub(in crate::team) intervention_cooldowns: HashMap<String, Instant>,
    /// Tracks recent escalation keys to suppress repeated alerts.
    pub(in crate::team) recent_escalations: HashMap<String, Instant>,
}

impl Scheduler {
    pub(in crate::team) fn new() -> Self {
        Self {
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_archive: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            pipeline_starvation_last_fired: None,
            planning_cycle_last_fired: None,
            planning_cycle_active: false,
            planning_cycle_consecutive_empty: 0,
            retro_generated: false,
            // Start far enough in the past to trigger an immediate check at startup.
            last_binary_freshness_check: None,
            // Run one retention pass shortly after startup.
            last_telemetry_retention: Instant::now()
                .checked_sub(Duration::from_secs(24 * 3600))
                .unwrap_or_else(Instant::now),
            // First sweep runs on the first tick after startup.
            last_tiered_inbox_sweep: None,
            intervention_cooldowns: HashMap::new(),
            recent_escalations: HashMap::new(),
        }
    }

    /// True while the cooldown recorded under `key` started less than
    /// `cooldown` ago.
    pub(in crate::team) fn cooldown_active(&self, key: &str, cooldown: Duration) -> bool {
        self.intervention_cooldowns
            .get(key)
            .is_some_and(|fired_at| fired_at.elapsed() < cooldown)
    }

    /// Returns true when `key` was already escalated within `window`;
    /// otherwise records it and returns false.
    pub(in crate::team) fn suppress_recent_escalation(
        &mut self,
        key: impl Into<String>,
        window: Duration,
    ) -> bool {
        let now = Instant::now();
        self.recent_escalations
            .retain(|_, seen_at| now.duration_since(*seen_at) < window);

        let key = key.into();
        if self.recent_escalations.contains_key(&key) {
            return true;
        }

        self.recent_escalations.insert(key, now);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_active_only_within_window() {
        let mut scheduler = Scheduler::new();
        assert!(!scheduler.cooldown_active("k", Duration::from_secs(60)));
        scheduler
            .intervention_cooldowns
            .insert("k".to_string(), Instant::now());
        assert!(scheduler.cooldown_active("k", Duration::from_secs(60)));
        assert!(!scheduler.cooldown_active("k", Duration::ZERO));
        assert!(!scheduler.cooldown_active("other", Duration::from_secs(60)));
    }

    #[test]
    fn suppress_recent_escalation_suppresses_repeats_within_window() {
        let mut scheduler = Scheduler::new();
        let window = Duration::from_secs(60);
        assert!(!scheduler.suppress_recent_escalation("k", window));
        assert!(scheduler.suppress_recent_escalation("k", window));
        assert!(!scheduler.suppress_recent_escalation("k", Duration::ZERO));
    }
}
