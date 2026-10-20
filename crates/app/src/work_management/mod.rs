//! Host-owned workflow state. No provider polling, filesystem access or dispatch.

pub mod reducer;
pub mod dispatch;
pub mod queue;
pub mod plan;
pub mod provider_adapter;
pub mod orchestrator;
pub use reducer::{ApplyOutcome, ProjectWorkState};

pub(crate) mod devices;
pub(crate) mod remote_execution;
pub(crate) mod repository_worker;
#[cfg(test)]
mod tests;
