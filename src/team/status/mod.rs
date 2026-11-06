//! Team status synthesis: runtime member status, health, metrics, and pane labels.
//!
//! Each submodule owns one concern; this file re-exports them so
//! `crate::team::status::X` paths stay stable.

mod board;
mod branch_signals;
mod health;
mod inbox;
mod members;
mod metrics;
mod pane_labels;
mod report;
mod subsystems;
mod supervisory;
#[cfg(test)]
mod tests;
mod workflow;

pub(crate) use board::*;
pub(crate) use branch_signals::*;
pub(crate) use health::*;
pub(crate) use inbox::*;
pub(crate) use members::*;
pub use metrics::*;
pub(crate) use pane_labels::*;
pub(crate) use report::*;
pub(crate) use subsystems::*;
pub(crate) use supervisory::*;
pub use workflow::*;
