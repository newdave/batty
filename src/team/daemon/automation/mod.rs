//! Periodic automation subsystems extracted from daemon.rs.
//!
//! Review timeout, dependency unblocking, pipeline starvation,
//! worktree reconciliation, board rotation, cron, retrospectives.
//!
//! Each submodule adds `impl TeamDaemon` methods for one concern; this file only
//! declares them and re-exports their shared helpers to siblings and tests.

mod aging;
mod claim_progress;
mod claims;
mod housekeeping;
mod ownership;
mod planning;
mod reconcile;
mod reviews;
#[cfg(test)]
mod tests;

use aging::*;
use claim_progress::*;
use claims::*;
use ownership::*;
use planning::*;
use reconcile::*;
use reviews::*;
