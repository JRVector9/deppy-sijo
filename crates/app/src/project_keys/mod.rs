//! Project key IO is owned by a bounded worker; render code receives metadata only.
pub(crate) mod agent_access;
mod build;
pub(crate) mod catalog;
pub mod cli;
pub mod contracts;
mod github;
pub(crate) mod process;
pub(crate) mod repository;
pub(crate) mod service;
pub(crate) mod session_capability;
mod sources;
#[cfg(test)]
pub(crate) mod tests;
pub mod worker;
