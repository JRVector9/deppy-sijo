//! storage crate (설계문서 9장). PR-11: append-only redacted 세션 로그.
//! SQLite metadata·rotation·encrypted raw log(기본 비활성 — 7장)는 후속 PR.

mod logs;

pub use logs::SessionLogWriter;
