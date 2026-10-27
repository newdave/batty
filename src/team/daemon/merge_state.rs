//! Daemon-owned merge state: the serial merge queue (including any in-flight
//! merge), deferred multi-repo direct merges, and per-task auto-merge
//! overrides set at runtime.

use std::collections::HashMap;
use std::path::Path;

use super::MergeQueue;
use crate::team::merge::DeferredDirectMerge;

#[derive(Default)]
pub(in crate::team) struct MergeState {
    /// Serial daemon-owned merge queue for auto-merge execution.
    pub(in crate::team) queue: MergeQueue,
    /// In-memory auto-merge overrides; take priority over the on-disk file.
    pub(in crate::team) overrides: HashMap<u32, bool>,
    /// Multi-repo direct merges waiting for the merge lock; retried each tick.
    pub(in crate::team) deferred_direct: Vec<DeferredDirectMerge>,
}

impl MergeState {
    #[cfg(test)]
    pub(in crate::team) fn set_auto_merge_override(&mut self, task_id: u32, enabled: bool) {
        self.overrides.insert(task_id, enabled);
    }

    /// In-memory override for `task_id`, falling back to the overrides file
    /// under `project_root`.
    pub(in crate::team) fn auto_merge_override(
        &self,
        project_root: &Path,
        task_id: u32,
    ) -> Option<bool> {
        if let Some(&value) = self.overrides.get(&task_id) {
            return Some(value);
        }
        let disk_overrides = crate::team::auto_merge::load_overrides(project_root);
        disk_overrides.get(&task_id).copied()
    }
}
