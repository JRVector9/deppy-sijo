//! Host-owned workflow state. No provider polling, filesystem access or dispatch.

pub mod dispatch;
pub mod orchestrator;
pub mod plan;
pub mod provider_adapter;
pub mod queue;
pub mod reducer;
pub(crate) mod task_worktree;
pub(crate) mod observation;
pub use reducer::{ApplyOutcome, ProjectWorkState};

pub(crate) mod devices;
pub(crate) mod remote_controller;
pub(crate) mod remote_execution;
pub(crate) mod remote_dispatch;
pub(crate) mod repository_worker;
#[cfg(test)]
mod tests;
