//! Project key IO is owned by a bounded worker; render code receives metadata only.
mod build;
pub mod cli;
pub mod contracts;
mod github;
pub(crate) mod process;
pub(crate) mod service;
mod sources;
#[cfg(test)]
pub(crate) mod tests;
pub mod worker;
