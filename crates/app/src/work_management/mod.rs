//! Host-owned workflow state. No provider polling, filesystem access or dispatch.

pub mod reducer;
pub use reducer::{ApplyOutcome, ProjectWorkState};

#[cfg(test)]
mod tests;
