//! Host-owned workflow state. No provider polling, filesystem access or dispatch.

pub mod reducer;
pub mod dispatch;
pub mod queue;
pub mod plan;
pub mod provider_adapter;
pub mod orchestrator;
pub use reducer::{ApplyOutcome, ProjectWorkState};

#[cfg(test)]
mod tests;
pub(crate) mod repository_worker;
