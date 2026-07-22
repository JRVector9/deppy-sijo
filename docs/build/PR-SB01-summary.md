# PR-SB01 settings boundary

## Boundary cutover

- `ui/agents.rs`, `ui/env_profiles.rs`, and `ui/credentials.rs` now render immutable, bounded, `Arc`-backed snapshots and return at most one intent per frame.
- Both large lists use `egui::ScrollArea::show_rows`; unchanged snapshots are reused for 300 frames without an adapter call.
- The leaves have no database, storage-row, keyring, secret-store, file-picker, filesystem, or process-environment dependency. Agent Sessions receives only key-presence metadata and emits a bounded save/delete intent.
- Secret inputs and revealed values are non-Clone/non-Serialize and omit secret-bearing Debug; revealed values are zeroized and retained under item/byte limits.
- All fourteen historical Agents/Environment boundary exceptions were deleted from xtask. No replacement exception was added.

## App host

- `app.rs` owns one lazy settings worker with a one-command/one-result queue, one worker-owned SQLite connection, a 30-second no-poll idle exit, and generation/revision stale-result rejection.
- Snapshot loading, agent registration/deletion/launch preparation, credential add/delete/reveal/orphan cleanup, API-key save/delete, legacy env deletion, dotenv writes, and project-path writes execute after render on that worker.
- Workspace projection is loaded under 256-item, 4 MiB aggregate, and 1 MiB row ceilings before
  materialization. Exact-path find-or-create and moved-path/anchor CAS use atomic storage
  transactions; Settings and sidebar folder selection return host intents and never open native
  dialogs or query SQLite from render.
- Ordinary project-path replacement also updates path plus optional dev/ino in one `IMMEDIATE`
  transaction, rejects a partial anchor before mutation, validates the bounded full-row
  postcondition before commit, and returns that committed projection to app memory.
- Inactive-workspace `.env` Resync is a Settings worker action using the shared redaction service;
  render only queues the operation. Inbox reply validation reads the bounded waiting projection,
  and render no longer calls `refresh_workspaces` or installs repaint polling timers.
- Credential creation registers a durable physical-slot staging row before keyring I/O and publishes metadata only after staging the complete access bundle. Deletion performs pointer-CAS metadata removal before exact bundle cleanup; cleanup failure leaves a recoverable Orphan row and never auto-retries an indeterminate mutation.
- Startup performs bounded ledger reconciliation and one-time logical access/refresh/DCR migration before constructing any runtime or secret-backed worker. A final inventory rejects any remaining logical pointer or missing Published ledger binding.
- A proxy agent launch is prepared without filesystem or runtime effects. A bounded launch ticket is reserved, the Unix datagram approval listener opens its database and binds before reporting Ready, and only then does the settings worker atomically write a config containing `--approval-notify-socket` before the runtime spawn.
- Approval snapshots are latest-only and materialized off-thread. There is no 500 ms watcher, periodic repaint, or frame-path approval database query.
- `AgentSpawnResolved` correlation is FIFO for the same workspace/config. Active, warm, and final-suspend event drains all observe correlation and exact session exit; unresolved session approvals are denied before the listener lease is released.
- Launch tickets, approval commands, and results are capped at eight. A missing correlation expires after 30 seconds without automatic retry.
- Non-proxy launches never start the approval listener. Constructing either lazy worker starts no thread.

## Verification

- Agents leaf: 5/5 focused tests passed.
- Environment leaf: 6/6 focused tests passed.
- Credentials leaf: 6/6 focused tests passed, including 300 unchanged frames with zero host calls and visible-row virtualization.
- Agent Sessions API-key bound/redaction and 300-frame zero-host-call regressions passed 1/1 each.
- Approval hub: 2/2 focused serial tests passed.
- Approval wake hub closeout: 5/5 tests passed, including cap-full shutdown and a removed published socket.
- Approval ticket tracker closeout: 4/4 tests passed, including SpawnSent deadline ownership and bounded denial backoff.
- Settings lazy/lifecycle construction: 2/2 tests passed.
- Settings workspace projection/transaction storage regressions: 6/6 passed; app exact-path adapter
  reuse passed 1/1.
- Atomic project-path+anchor focused regressions: 3/3; storage full suite: 159/159.
- Inactive dotenv worker adapter and committed project-path projection: 1/1 each.
- Startup secret migration and physical credential deletion: 2/2 tests passed.
- Mandatory proxy notify socket: 1/1 test passed.
- App all-target check and strict Clippy pass with zero warnings after the bounded-dotenv and credential/API-key closeout.
- `cargo run -q -p xtask -- check-boundary`: passed with zero explicit UI boundary exceptions.
- `cargo run -q -p xtask -- check-deps`: passed for 23 crates.
- Workspace rustfmt and diff-check passed before the bounded-dotenv follow-up began.

The managed sandbox intermittently denied local socket binds. The unchanged approval listener test passed when rerun serially; no product/test bypass was added.
