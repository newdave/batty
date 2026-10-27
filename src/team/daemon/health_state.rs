//! Agent health bookkeeping: backend health and quota parking, narration and
//! context-pressure trackers, and the timers that gate periodic health checks.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use super::health::context::ContextPressureTracker;
use super::health::narration::NarrationTracker;
use crate::agent::BackendHealth;

pub(in crate::team) struct HealthState {
    /// Per-member agent backend health state.
    pub(in crate::team) backend: HashMap<String, BackendHealth>,
    /// Per-member quota retry-at deadline (epoch seconds). Populated when a
    /// shim emits `Event::QuotaBlocked` with a retry_at. Consulted by the
    /// backend-health transition, dispatch selection, and aging alerts to
    /// keep an engineer marked as `quota_exhausted` (i.e. parked) until the
    /// reset deadline passes. A successful poll_shim ping is NOT evidence
    /// of quota recovery — only the deadline or operator intervention is.
    pub(in crate::team) quota_retry_at: HashMap<String, u64>,
    /// Rolling capture history used to detect narration loops.
    pub(in crate::team) narration: NarrationTracker,
    /// Per-session output tracking used for proactive context-pressure handling.
    pub(in crate::team) context_pressure: ContextPressureTracker,
    /// #690: When each member most recently transitioned Idle→Working.
    /// Cleared when they transition back to Idle. Consulted by the
    /// zero-output restart check so an agent that JUST became Working
    /// (e.g. an inbox message arrived 8s before the poll tick) is not
    /// killed for lifetime-zero-output on a shim that has been Idle
    /// for the preceding 18 minutes.
    pub(in crate::team) working_since: HashMap<String, Instant>,
    /// When the last periodic health check was run.
    pub(in crate::team) last_health_check: Option<Instant>,
    /// When the last shim health check (Ping) was sent.
    pub(in crate::team) last_shim_health_check: Instant,
    /// Rate-limiting: last time each engineer received an uncommitted-work warning.
    pub(in crate::team) last_uncommitted_warn: HashMap<String, Instant>,
    /// Last time the daemon checked for stale per-worktree cargo targets to prune.
    pub(in crate::team) last_shared_target_cleanup: Option<Instant>,
    /// Last time the daemon ran a full disk hygiene pass.
    pub(in crate::team) last_disk_hygiene_check: Option<Instant>,
}

impl HealthState {
    pub(in crate::team) fn new(
        narration: NarrationTracker,
        context_pressure: ContextPressureTracker,
    ) -> Self {
        Self {
            backend: HashMap::new(),
            quota_retry_at: HashMap::new(),
            narration,
            context_pressure,
            working_since: HashMap::new(),
            // Start far enough in the past to trigger an immediate check.
            last_health_check: None,
            last_shim_health_check: Instant::now(),
            last_uncommitted_warn: HashMap::new(),
            last_shared_target_cleanup: None,
            last_disk_hygiene_check: None,
        }
    }

    /// A member is parked when either (a) cached health is `QuotaExhausted`
    /// or (b) the tracked quota `retry_at` deadline is still in the future.
    /// Dispatch selection, stall-timer reclaim, and aging-alert escalation
    /// gate on this to avoid churn against an engineer that simply cannot
    /// make progress until the quota window elapses (#674).
    pub(in crate::team) fn member_backend_parked(
        &self,
        project_root: &Path,
        member_name: &str,
    ) -> bool {
        if project_root
            .join(".batty/backend-configuration")
            .join(format!("{member_name}.json"))
            .exists()
        {
            return true;
        }
        if matches!(
            self.backend.get(member_name),
            Some(
                BackendHealth::QuotaExhausted
                    | BackendHealth::AuthRequired
                    | BackendHealth::ConfigurationRequired
            )
        ) {
            return true;
        }
        if let Some(&retry_at) = self.quota_retry_at.get(member_name) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d: std::time::Duration| d.as_secs())
                .unwrap_or(0);
            if retry_at > now {
                return true;
            }
        }
        false
    }
}
