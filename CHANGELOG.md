# Changelog

## [0.19.0] - 2026-10-24

- Add direct project API registration from Notes and terminal menus with a compact key editor and explicit metadata reload.
- Let new local Claude, Codex, Cursor and Grok sessions discover explicitly permitted project APIs and execute with current selected keys, scoped authorization and redacted output.
- Include the reviewed SSH recovery, launcher and usage fixes merged into main; package matching versioned SSH helpers.
- Signed local developer build; no automatic app restart.

## [0.15.1] - 2026-10-20

- Show each main agent and worker provider once in project settings while preserving every model and reasoning-effort option.
- Local signed development rebuild on the separate Command Center branch.

## [0.15.0] - 2026-10-20

- Keep the main composer editable before binding, configure the main agent from the composer, and preserve drafts through configuration and late replies.
- Restore approved native/web desktop and mobile layout dimensions, combined task/results and scoped history rows and filters.
- Local signed development build on the separate Command Center branch.

## Unreleased — Command Center v2 development branch

- Add an opt-in project/main-agent/worker layout with independent task attempts, drafts, approvals, results, Git and history routes; retain the original layout and source rollback point.
- Persist work in a separate sidecar and admit local or explicitly granted remote workers through durable operation identities. Recover uncertain remote receipts by read-only lookup without replaying work.
- Add pinned, project-scoped TLS worker connections, exact owned-worker termination, and read-only remote terminal views in the PWA and paired Relay shell.
- Preserve mobile remote scroll positions and resume read-only observation after lost receipts while retaining mutating-operation fences.
- Restore the approved native row, drawer, footer and settings dimensions, and the PWA monitor, MAIN/CHECK/LIVE and exact terminal navigation. Preserve focused controls and scroll positions across snapshots.
- Keep per-goal review evidence with explicit current-policy reconfirmation after recovery, preserve productive attempts, and evict only clean completed project caches.
- Keep the main composer editable before an agent is connected; configure real sessions and provider/model/effort choices from the composer, retain drafts across binding, and require a separate explicit send.
- Match approved desktop/mobile reading surfaces, true semibold typography, compact controls and combined work/result tabs; preserve newer drafts and settings across late load, save and input acknowledgements.
- Preserve actual history categories with legacy-compatible storage, exact task ownership and compact desktop/mobile rows and filters.
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
