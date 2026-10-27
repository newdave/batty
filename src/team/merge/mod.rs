//! Merge orchestration extracted from the team daemon.
//!
//! This module owns the completion path after an engineer reports a task as
//! done in a worktree-based flow. It validates that the branch contains real
//! work, runs the configured test gate, serializes merges through the daemon
//! merge queue (or a non-blocking lock for multi-repo projects), and either
//! lands the branch on `main` or escalates conflicts and failures back
//! through the daemon.
//!
//! The daemon calls into this module so the poll loop can stay focused on
//! orchestration while merge-specific retries and board transitions remain in
//! one place.

mod completion;
mod git_ops;
mod lock;
mod operations;

#[cfg(test)]
pub(crate) use completion::handle_engineer_completion_and_wait;
pub(crate) use completion::{
    DeferredDirectMerge, VerificationJob, append_merge_learning, enqueue_reviewed_merge,
    handle_engineer_completion, multi_repo_changed_paths, poll_deferred_direct_merges,
    poll_verification_jobs, record_merge_test_timing, restore_deferred_direct_merges,
};
#[cfg(test)]
pub(crate) use lock::acquire_eventually_for_test;
pub(crate) use lock::{
    MergeLock, MergeMode, MergeOutcome, infer_merge_mode_from_failure, merge_lock_holder_pid,
    merge_lock_is_held, merge_lock_path,
};
pub(crate) use operations::{
    RootCheckoutDirty, RootDirtyState, inspect_root_dirty_state, merge_engineer_branch_into_trunk,
};
