# Changelog

## Unreleased — Command Center v2 development branch

- Add an opt-in project/main-agent/worker layout with independent task attempts, drafts, approvals, results, Git and history routes; retain the original layout and source rollback point.
- Persist work in a separate sidecar and admit local or explicitly granted remote workers through durable operation identities. Recover uncertain remote receipts by read-only lookup without replaying work.
- Add pinned, project-scoped TLS worker connections, exact owned-worker termination, and read-only remote terminal views in the PWA and paired Relay shell.
- Preserve mobile remote scroll positions and resume read-only observation after lost receipts while retaining mutating-operation fences.
- Restore the approved native row, drawer, footer and settings dimensions, and the PWA monitor, MAIN/CHECK/LIVE and exact terminal navigation. Preserve focused controls and scroll positions across snapshots.
- Keep per-goal review evidence with explicit current-policy reconfirmation after recovery, preserve productive attempts, and evict only clean completed project caches.
- Bind supported structured command approvals to the exact live owner, turn and provider request, fence actual writes against revocation, and defer unsupported execution scopes to the original session.

This branch has no distributed release artifact. See [usage and capability boundaries](docs/command-center-v2.md) and [verification evidence](docs/qa/2026-10-20-command-center-v2-implementation.md).

## [0.13.0] - 2026-10-15

### Added

- Opt in per cloud connection and shared session to return verified local Codex turn completion through the original initialized MCP request. Input permissions and control leases remain separate.
- Preserve bounded private completion receipts and reconnect only the original host session, with distinct local and delivery states. Providers without native completion evidence reject tracked submission before input.

### Fixed

- Fence completion against intervening input, runtime event loss, revoked read access, replaced sessions, expired watches, and late admission replies.
- Bound deferred transport resources and redact newly registered secrets before each original response or replay.

## [0.12.3] - 2026-10-15

### Fixed

- Preserve Hangul composition while a reserved terminal focus is applied, including delayed native commits.
- Deliver MCP authentication and busy error responses before bounded unread-request cleanup closes the connection.
- Make the document external-edit conflict regression deterministic without weakening conflict or cleanup checks.

## [0.12.2] - 2026-10-15

### Added
- Manage cloud agent connections, session permissions, control leases, tasks, history, and recovery from the approved settings tabs.
- Open cloud task results and permitted actions from the mobile PWA.

### Fixed

- Upgrade rustls to 0.23.46 for RUSTSEC-2026-0346/0347/0348/0349.
- Keep terminal input tied to the live editor while browsing scrollback; preserve wide character cursor alignment.
- Bound app-server writes and shutdown, retire stale terminal query replies, and clean up owned foreground processes.
- Preserve document ownership, unsaved buffers, save conflicts, symlinks, and popup input ownership.
- Keep file deletion and clipboard operations scoped to the intended selection and directory.
- Preserve WAL data in migration backups and order queued session persistence correctly.
- Recover failed cloud history initialization and finish accepted work without admitting new work during shutdown.
- Revoke retained read permissions after a session ends and mask cloud metadata and wrapped secret fragments.
- Preserve approved MCP OAuth names and show the resolved Claude model version without changing launch aliases.

### Changed
- Reuse safe cloud answer projections and process lookup indexes to reduce repeated work.
- Match context menu spacing and cloud settings sizing to the approved designs.
