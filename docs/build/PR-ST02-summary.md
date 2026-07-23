# PR-ST02 — Settings Write Admission

## Outcome

PR-ST02 closes the gap between bounded Settings reads and previously unbounded production writes.
Every supported Settings inventory is now admitted before mutation inside the same SQLite
`IMMEDIATE` transaction that performs the insert, update, or upsert. A failed item, row-byte,
aggregate-byte, scope, type, or overflow check therefore changes zero rows.

This milestone adds no thread, timer, polling loop, network work, async runtime, schema migration,
dependency, boundary allowlist, or public service API. Admission runs only for an actual Settings
write. Existing immutable snapshot reads and render behavior are unchanged.

## Limits and snapshot laws

The write side uses the already-frozen storage-owned Settings ceilings; no limit was raised.

| Inventory or projection | Production ceiling |
| --- | ---: |
| Workspaces | 256 rows |
| Agent configurations | 1,024 live rows |
| Environment profiles | 256 rows per workspace |
| Environment variables | 4,096 rows per workspace |
| Visible credentials | 4,096 rows per workspace snapshot |
| Enabled MCP backend summaries | 256 rows |
| One retained row | 1 MiB logical bytes |
| One complete Settings snapshot | 4 MiB logical bytes |
| Agent argument inventory | Existing 256-item / 64-KiB validation remains in force |

Workspace admission also proves that the new workspace's empty Agent and Environment views are
readable. Agent insertion validates the global Agent inventory together with every workspace's
profile usage and the enabled-backend inventory. A global credential is visible in every
workspace, so its admission validates each affected Environment view with that workspace's local
credentials, profiles, and variables.

Logical byte accounting uses the same retained fields as the corresponding read snapshot. All
new admission errors are fixed low-cardinality codes and contain no names, paths, environment
values, credential coordinates, arguments, or other caller data.

## Production write ownership

`storage::Db` remains the only public API owner. No admission helper is exported. The following
existing production methods now own admission and mutation in one transaction:

- `ensure_default_workspace`, `create_workspace`, and
  `find_or_create_workspace_by_exact_path`;
- `rename_workspace`, `set_workspace_path`, `set_workspace_anchor`,
  `set_workspace_path_and_anchor`, and `update_workspace_moved_path_cas`;
- legacy `insert_credential`;
- `insert_credential_with_secret_slot` and
  `insert_credential_with_secret_slot_revision_cas`, through their common transaction helper;
- `rotate_credential_secret_slot`, `publish_legacy_credential_secret_slot_cas`,
  `publish_credential_secret_slot_cas`, and `publish_credential_secret_slot_revision_cas`, through
  their common physical-slot publication helper whenever `masked_hint` will change;
- `insert_env_profile`;
- `upsert_env_var`;
- `insert_agent_config`.

An exact-path workspace lookup still returns its existing row at the cap. Environment variable
upsert excludes the exact `(profile_id, key)` candidate from the inventory probe, so an update at
the item ceiling remains legal while a new key fails before the UPSERT. Duplicate credential IDs
and missing foreign keys keep their prior SQLite constraint semantics rather than being rewritten
as capacity errors.

Workspace updates load the exact bounded current row, substitute the proposed name/path/anchor,
and admit the complete candidate before the first UPDATE. Moved-path CAS still returns `Stale`
without mutation when the expected path or anchor does not match. Physical credential publication
loads the exact pointer-matched metadata and admits the proposed masked hint before orphaning the
previous published slot or updating credential/OAuth/ledger state. A stale pointer may still move
only its unused staging slot to `Orphan`; it does not have a Settings candidate to admit.
The complete candidate byte count is compared with the 1-MiB row ceiling before
`masked_hint.to_owned()`; malformed oversized borrowed input therefore cannot force a second
unbounded allocation before rejection.

## Constant-query resource behavior

The first correct-but-expensive implementation queried each of as many as 256 workspaces while
admitting a global Agent or credential. It was not accepted. The final design uses a constant
number of SQL statements:

- Agent admission combines one global Agent probe, one backend probe, one workspace-scope probe,
  and one grouped profile-extrema probe.
- Global credential admission performs one bounded grouped Environment preflight after the
  workspace-scope probe and returns at most 256 real scope rows, or one synthetic empty scope when
  no workspace exists.
- Bounded input subqueries may scan only the existing hard product ceilings plus one. Only grouped
  usage is materialized, capped at 257 rows; a raw million-row CTE is not retained.

A source-law regression requires the grouped preflights and rejects reintroduction of the former
workspace-by-workspace query loop. Concurrent workspace writers are serialized by the same
`IMMEDIATE` transaction and exactly one writer can acquire the final slot.

## Verification

- `cargo test -p storage --no-fail-fast` — 233/233 unit tests passed; doc-tests passed.
- Final Settings-focused suite — 19/19 passed.
- Item `N+1` coverage passes for workspace, visible credential, profile, variable, and Agent
  inventories.
- Exact-path-at-cap reuse, env-var update-at-cap, Agent logical-byte `+1`, and concurrent final-slot
  regressions pass.
- One exact 4-MiB workspace fixture proves rename, path, anchor, atomic path/anchor, and moved-path
  CAS all reject aggregate `+1` before a trigger can observe UPDATE.
- Physical-slot publication proves a masked-hint row-byte `+1` leaves the logical pointer, OAuth
  metadata, masked hint, and staged ledger row unchanged. Source laws cover all four
  rotation/publication entrypoints and require common admission before pointer orphaning or
  credential UPDATE.
- `cargo check -p storage --all-targets` — passed.
- `cargo clippy -p storage --all-targets -- -D warnings` — passed without a new allow attribute.
- Storage-scoped Rust 2024 rustfmt and `git diff --check` — passed.
- The complete storage suite includes the database plaintext-secret and secret-like persistence
  regressions; both remain green.

The workspace-wide fmt command encountered another parallel lane's temporarily unformatted
`crates/app/src/app.rs` edit. The storage file itself was clean and that unrelated file was not
modified by this lane.

## Failed approaches and corrections

- The first test build attempted to decode SQLite `COUNT(*)` directly as `usize` and passed one
  owned path instead of borrowing it. Tests now decode `i64`, checked-convert to `usize`, and borrow
  the path.
- A pre-existing read-boundary fixture used the public writer to construct a deliberately
  read-invalid aggregate. Once writes became fail-closed, the fixture correctly had to seed that
  corrupt boundary directly through test-only SQL.
- The initial Agent helper exceeded the strict Clippy argument limit. Its non-public candidate
  fields are now grouped in a typed, non-Debug input rather than hidden with an allow attribute.
- The initial global admission performed workspace-count-scaled SQL. It was replaced with grouped
  constant-query validation, then tightened again so only grouped rows—not up to roughly one
  million raw rows—are materialized.
- The first source-law assertion matched its own sentinel string. It now checks the exact expected
  occurrence and passed.
- Independent review found that the first ST02 cut covered creates but not five workspace update
  APIs or masked-hint changes during credential slot publication. Those paths now share typed
  pre-mutation admission; the former update-then-postcondition behavior remains only as a defensive
  postcondition after the new admission.
- Two source-law reruns initially targeted a wrapper moved by the credential refactor and counted
  their own helper sentinel. The final laws inspect the actual candidate helper and each public
  rotation/CAS body directly; no production workaround was introduced.
- Independent final review found the first masked-hint fix summed borrowed bytes but deferred the
  actual 1-MiB comparison until after cloning into `CredentialMeta`. The helper now checks the
  computed candidate size before the clone, and its source law requires that ordering.

## Deterministic completion versus release measurement

ST02 is deterministically complete: transaction rollback, exact/plus-one limits, concurrency,
static errors, constant-query structure, compilation, strict linting, and storage regressions are
proved locally. It does not by itself approve production hardware performance.

The 30-minute RSS/thread/socket/queue slope run and release Scenario A–E CPU, RSS, and terminal
frame-p95 measurements remain PR-BG01 release gates. They must run on the final integrated build
after all structural development is frozen. No soak result is substituted for the deterministic
ST02 invariants, and no hardware claim is made in this milestone.
