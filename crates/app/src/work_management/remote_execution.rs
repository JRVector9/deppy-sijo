//! Durable remote admission is implemented by the actual bounded journal worker.
#[path = "remote_journal.rs"]
pub(crate) mod journal;
pub(crate) use journal::AsyncRemoteJournal;
