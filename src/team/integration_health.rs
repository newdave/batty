//! Consecutive-failure tracking for chat integration pollers.
//!
//! The Telegram and Discord bridges swallow poll errors so one bad poll never
//! aborts the daemon tick. This tracker makes a *sustained* outage visible: it
//! reports a transition after [`UNHEALTHY_THRESHOLD`] consecutive failures and
//! again when a poll succeeds afterwards.

/// Consecutive failed polls before an integration is reported unhealthy.
pub const UNHEALTHY_THRESHOLD: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthTransition {
    /// The failure streak just reached the threshold.
    BecameUnhealthy { failures: u32 },
    /// A poll succeeded after the integration was reported unhealthy.
    Recovered { failures: u32 },
}

#[derive(Debug, Default, Clone)]
pub struct PollHealth {
    consecutive_failures: u32,
    unhealthy: bool,
}

impl PollHealth {
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Record a failed poll. Returns a transition only on the poll that
    /// crosses the threshold (not on every later failure).
    pub fn record_failure(&mut self) -> Option<HealthTransition> {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if !self.unhealthy && self.consecutive_failures >= UNHEALTHY_THRESHOLD {
            self.unhealthy = true;
            return Some(HealthTransition::BecameUnhealthy {
                failures: self.consecutive_failures,
            });
        }
        None
    }

    /// Record a successful poll. Returns a transition only if the integration
    /// had been reported unhealthy.
    pub fn record_success(&mut self) -> Option<HealthTransition> {
        let failures = std::mem::take(&mut self.consecutive_failures);
        if std::mem::take(&mut self.unhealthy) {
            return Some(HealthTransition::Recovered { failures });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_unhealthy_exactly_at_threshold() {
        let mut health = PollHealth::default();
        for _ in 0..UNHEALTHY_THRESHOLD - 1 {
            assert_eq!(health.record_failure(), None);
        }
        assert_eq!(
            health.record_failure(),
            Some(HealthTransition::BecameUnhealthy {
                failures: UNHEALTHY_THRESHOLD
            })
        );
        // Further failures do not re-report.
        assert_eq!(health.record_failure(), None);
        assert_eq!(health.consecutive_failures(), UNHEALTHY_THRESHOLD + 1);
    }

    #[test]
    fn success_before_threshold_is_silent_and_resets_streak() {
        let mut health = PollHealth::default();
        for _ in 0..UNHEALTHY_THRESHOLD - 1 {
            health.record_failure();
        }
        assert_eq!(health.record_success(), None);
        assert_eq!(health.consecutive_failures(), 0);
        for _ in 0..UNHEALTHY_THRESHOLD - 1 {
            assert_eq!(health.record_failure(), None);
        }
    }

    #[test]
    fn recovery_reported_once_after_unhealthy() {
        let mut health = PollHealth::default();
        for _ in 0..7 {
            health.record_failure();
        }
        assert_eq!(
            health.record_success(),
            Some(HealthTransition::Recovered { failures: 7 })
        );
        assert_eq!(health.record_success(), None);
    }

    #[test]
    fn can_become_unhealthy_again_after_recovery() {
        let mut health = PollHealth::default();
        for _ in 0..UNHEALTHY_THRESHOLD {
            health.record_failure();
        }
        health.record_success();
        for _ in 0..UNHEALTHY_THRESHOLD - 1 {
            assert_eq!(health.record_failure(), None);
        }
        assert!(matches!(
            health.record_failure(),
            Some(HealthTransition::BecameUnhealthy { .. })
        ));
    }
}
