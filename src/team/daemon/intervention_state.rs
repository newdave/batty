//! Per-member intervention bookkeeping: idle nudge schedules, triage and
//! owned-task intervention epochs, and review-aging tracking.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::interventions::{NudgeSchedule, OwnedTaskInterventionState};
use crate::team::standup::MemberState;

pub(in crate::team) struct InterventionState {
    /// Idle nudge schedules, keyed by member name.
    pub(in crate::team) nudges: HashMap<String, NudgeSchedule>,
    pub(in crate::team) triage_idle_epochs: HashMap<String, u64>,
    pub(in crate::team) triage_interventions: HashMap<String, u64>,
    pub(in crate::team) owned_task_interventions: HashMap<String, OwnedTaskInterventionState>,
    pub(in crate::team) review_first_seen: HashMap<u32, u64>,
    pub(in crate::team) review_nudge_sent: HashSet<u32>,
}

impl InterventionState {
    pub(in crate::team) fn new(nudges: HashMap<String, NudgeSchedule>) -> Self {
        Self {
            nudges,
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            review_first_seen: HashMap::new(),
            review_nudge_sent: HashSet::new(),
        }
    }

    pub(in crate::team) fn update_nudge_for_state(
        &mut self,
        member_name: &str,
        new_state: MemberState,
    ) {
        if let Some(schedule) = self.nudges.get_mut(member_name) {
            match new_state {
                MemberState::Idle => {
                    if schedule.paused || schedule.idle_since.is_none() {
                        schedule.idle_since = Some(Instant::now());
                        schedule.fired_this_idle = false;
                    }
                    schedule.paused = false;
                }
                MemberState::Working => {
                    schedule.idle_since = None;
                    schedule.fired_this_idle = false;
                    schedule.paused = true;
                }
            }
        }
    }

    pub(in crate::team) fn update_triage_intervention_for_state(
        &mut self,
        member_name: &str,
        new_state: MemberState,
    ) {
        match new_state {
            MemberState::Working => {
                self.triage_idle_epochs
                    .entry(member_name.to_string())
                    .or_insert(0);
            }
            MemberState::Idle => {
                let had_epoch = self.triage_idle_epochs.contains_key(member_name);
                let epoch = self
                    .triage_idle_epochs
                    .entry(member_name.to_string())
                    .or_insert(0);
                if had_epoch {
                    *epoch += 1;
                }
            }
        }
    }

    /// Drop owned-task intervention streaks recorded under `keys`.
    pub(in crate::team) fn clear_owned_task_streaks(&mut self, keys: &[String]) {
        for key in keys {
            self.owned_task_interventions.remove(key);
        }
    }
}
