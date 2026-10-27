//! Completion-verification bookkeeping: per-engineer verification loops,
//! background test jobs, the periodic main smoke test, and the counters that
//! break empty-completion loops.

use std::collections::HashMap;
use std::time::Instant;

use super::MainSmokeState;
use super::background_tests::BackgroundTests;
use crate::team::verification::VerificationState;

pub(in crate::team) struct VerificationTracking {
    /// Per-engineer completion verification loop state.
    pub(in crate::team) engineers: HashMap<String, VerificationState>,
    /// Test commands running off the poll loop (verification, main smoke,
    /// chat merges).
    pub(in crate::team) background_tests: BackgroundTests,
    /// Latest periodic main smoke-test outcome.
    pub(in crate::team) main_smoke_state: Option<MainSmokeState>,
    pub(in crate::team) last_main_smoke_check: Instant,
    /// Tracks consecutive narration-only rejections per task (commits exist
    /// but the branch still has no file diff). After threshold, escalates.
    pub(in crate::team) narration_rejection_counts: HashMap<u32, u32>,
    /// Tracks consecutive shim completions whose worktree still has zero diff
    /// against main, so the daemon can break completion loops proactively.
    pub(in crate::team) zero_diff_completion_counts: HashMap<u32, u32>,
}

impl VerificationTracking {
    pub(in crate::team) fn new() -> Self {
        Self {
            engineers: HashMap::new(),
            background_tests: BackgroundTests::default(),
            main_smoke_state: None,
            last_main_smoke_check: Instant::now(),
            narration_rejection_counts: HashMap::new(),
            zero_diff_completion_counts: HashMap::new(),
        }
    }

    /// True while a broken main smoke test pauses dispatch.
    pub(in crate::team) fn dispatch_paused_by_main_smoke(&self) -> bool {
        self.main_smoke_state
            .as_ref()
            .is_some_and(|state| state.broken && state.pause_dispatch)
    }

    /// Count another zero-diff completion for `task_id`; returns the new count.
    pub(in crate::team) fn note_zero_diff_completion(&mut self, task_id: u32) -> u32 {
        let count = self.zero_diff_completion_counts.entry(task_id).or_insert(0);
        *count += 1;
        *count
    }

    pub(in crate::team) fn clear_zero_diff_completion(&mut self, task_id: u32) {
        self.zero_diff_completion_counts.remove(&task_id);
    }
}
