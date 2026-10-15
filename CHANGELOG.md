# Changelog

## [0.12.2] - 2026-10-15

### Added
- Manage cloud agent connections, session permissions, control leases, tasks, history, and recovery from the approved settings tabs.
- Open cloud task results and permitted actions from the mobile PWA.

### Fixed
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
