# PR-SC01 — Versioned Secret Slots and Bounded Redaction

## 2026-07-22 consuming secret-transfer amendment

`SecretString::into_string` transfers the existing plaintext allocation with `mem::take`, avoiding
the compatibility adapter's extra plaintext clone while making the caller responsible for moving
the value promptly into another zeroizing owner. `SecretString` remains non-Clone, non-Serialize,
and redacted in Debug. Pointer/capacity preservation and the actual volatile Drop-zeroization path
are regression-tested. Root verification passes secret 46/46 plus doc-tests, check, strict Clippy,
package fmt, and diff-check.

## Outcome

PR-SC01 adds the typed keyring and redaction primitives required for the atomic Connector cutover.
OAuth access, refresh, and DCR values can be staged in an isolated versioned physical slot before
SQLite publishes that slot. Rotating redaction patterns now have bounded item/byte retention,
reference-counted execution leases, grace expiry, and fail-closed acquisition.

This PR does not change a production caller. `app.rs` must compose these APIs with the ST01 storage
transaction during PR-IN01; keeping a second legacy production path after cutover is not intended.

## Typed physical slot protocol

- `LogicalCredentialId` and `PhysicalSecretSlot` are distinct types. A physical slot is encoded as
  `deppy.oauth.v1.<logical-id-hex>.<uuid>` and is validated before use.
- `SecretBundle` and `SecretBundleRef` contain access/refresh/DCR values and are intentionally
  non-Clone and non-Serialize. Their `Debug` output is always redacted.
- `SecretBundleStagePlan` rejects a new or previous slot owned by another logical credential and
  rejects reusing the current physical slot.
- `stage_secret_bundle` confirms the random new slot is empty, writes its access/refresh/DCR
  entries, and leaves the database pointer untouched. On any error it deletes every entry the
  attempt could have touched, including a backend `write-then-error` result.
- `delete_secret_bundle` is idempotent. `list_secret_bundle_slots` inventories only the versioned
  prefix. `reconcile_orphan_secret_slots` deletes unreferenced versioned slots at startup without a
  timer or polling loop.
- `KeyringSecretStore::list_secret_ids` uses the platform keyring inventory restricted to
  `KEYRING_SERVICE`. A backend without enumeration support returns an error instead of pretending
  there are no orphans.
- `SecretString`, retained redaction pattern bytes, and streaming redactor carry overwrite owned
  byte allocations on drop.

### v30 legacy-source cleanup amendment

- `physical_secret_slot_ledger.legacy_cleanup_username` is a nullable durable cleanup marker on the
  existing bounded ledger row. It stores only the exact legacy base username; access, `.refresh`,
  and `.dcr` values never enter SQLite. The marker is constrained to the logical credential id,
  unique across live obligations, NUL-free, and included in the existing 4,096-item/1 MiB ledger
  ceiling.
- `Db::publish_legacy_credential_secret_slot_cas` is the only API that creates the marker. An
  IMMEDIATE transaction verifies the expected legacy pointer, publishes staging to the versioned
  physical slot, updates OAuth metadata, and sets the marker atomically. Regular credential
  creation and rotation never set it. A stale pointer leaves the live pointer unchanged and moves
  only the new staging slot to orphan.
- `physical_secret_slots_for_reconciliation` preflights deterministic `(created_at, physical_slot)`
  item and aggregate byte totals before materializing rows, then returns the redacted marker field.
- After exact keyring deletion, `acknowledge_legacy_secret_source_deleted(logical, published_slot,
  expected_legacy)` clears only that marker and is idempotent. A marker-bearing orphan cannot be
  acknowledged as physically deleted until legacy cleanup acknowledgement succeeds.
- v30 migration tests cover forward upgrade, DDL rollback with `user_version=29` retained, duplicate
  and malformed marker rejection, and all historical migration prefixes.

## OAuth stage and refresh contract

- `auth::stage_oauth_token_bundle` stages an `OAuthToken` and optional DCR secret without cloning a
  secret-bearing value.
- `auth::refresh_access_token_for_slot` accepts typed logical/current-slot coordinates and rejects
  a mismatched slot before keyring or network I/O.
- The current bundle is read once. The refresh exchange preserves an existing refresh token when
  the provider omits a replacement, and the already-read DCR secret is borrowed into the publish
  callback instead of being fetched again.
- The mandatory publish callback runs inside the credential single-flight critical section. It
  must stage the new bundle and commit the SQLite pointer/metadata transaction before returning
  `Ok(())`. A waiter receives `AlreadyRefreshed` only after that commit succeeds.
- Publish failure is shared as failure, never as refresh success. A deterministic two-thread test
  blocks the leader in publish, proves the waiter cannot finish early, injects commit failure, and
  proves neither thread reports success.
- A rejected or uncertain exchange does not delete or retry a typed slot. Reauthorization or a new
  operation must explicitly publish a replacement.
- The existing logical-username refresh API remains source-compatible until the atomic app
  cutover; it is not the target production path.

## Bounded redaction lifecycle

Production defaults are 4,096 retained patterns, 4 MiB of retained pattern bytes, a 32 KiB maximum
single registration input derived from that byte ceiling, and a 30-second post-use grace period.

- `register_permanent` is checked and rejects secrets too short or large to protect.
- `acquire_execution_lease` atomically covers all secrets for one external operation. It must
  succeed before that operation starts, and the non-Clone `RedactionLease` must remain alive until
  transport output has been flushed.
- Input secrets and encoded variants are incrementally deduplicated in bounded `BTreeSet`s. Unique
  item/byte counts are checked while variants are generated, so temporary RAM cannot grow past the
  corpus ceilings before retained-capacity validation.
- Capacity failure leaves the corpus unchanged. Legacy void registration switches the service to
  whole-output fail-closed mode when it cannot safely retain a pattern.
- Rotating entries carry reference counts. The last lease drop starts grace expiry. Expired entries
  are removed during registration, matching, flush, or stats reads; no cleanup thread, timer,
  network request, polling, or repaint is created.
- `RedactionCorpusStats` exposes only bounded counts/bytes/lease state. Pattern bytes and secret
  values are not serializable or printable.

## Failure and resource coverage

Deterministic tests cover:

- a DCR write failure and a backend write-then-error both rolling the whole new slot back;
- startup reconciliation retaining one referenced version while deleting three orphans;
- 100 keyring rotations retaining one physical slot and exactly three entries;
- 100 rotating token generations returning redaction item/byte counts to the permanent baseline;
- two overlapping leases, grace-period matching, and match-time expiry;
- short, oversized, item-limit, and byte-limit acquisition failing closed with no partial corpus;
- 256 distinct secret inputs stopping at requested item 33 for a limit of 32, and 10,000 duplicate
  inputs sharing one bounded variant set;
- oversized legacy JSON being rejected before serde parsing and switching to fail-closed output;
- typed refresh missing-token, wrong-slot, DCR preservation, and publish-failure races.

These tests prove bounded retained keyring entries and redaction bytes, which are the deterministic
SC01 memory invariant. Process RSS is allocator- and platform-dependent; PR-OD01 must measure its
30-minute slope together with thread/socket/queue slopes instead of adding a flaky RSS unit-test
threshold here.

The v30 storage amendment additionally covers pre-publish crash recovery, atomic marker/pointer
rollback, post-commit restart, exact three-source acknowledgement, acknowledgement failure,
stale-pointer orphaning, 4,097-row and 1 MiB preflight rejection, and regular rotation producing no
marker.

## Verification

- `cargo test -p secret --no-fail-fast` — pass (34 tests; doc-tests pass).
- `cargo test -p auth --no-fail-fast -- --test-threads=1` — pass (58 tests; doc-tests pass).
- `cargo check -p secret -p auth` — pass.
- `cargo clippy -p secret -p auth --all-targets -- -D warnings` — pass.
- `cargo run -q -p xtask -- check-deps` — pass (23 crates, zero forbidden edges/cycles).
- `cargo run -q -p xtask -- check-boundary` — pass with the existing 53 exceptions; no allowlist
  entry was added.
- `cargo run -q -p xtask -- security-scan` — boundary/dependency, storage secret persistence 4/4,
  mcp-store secret persistence 2/2, and audit 37/37 pass. The aggregate cannot finish in this
  managed sandbox because MCP HTTP fixtures are denied at local `TcpListener::bind` with
  `Operation not permitted`; the storage persistence scans pass before the command stops.
- Scoped rustfmt and `git diff --check` — pass.
- `cargo test -p storage --no-fail-fast -- --test-threads=1` — pass (132 tests; doc-tests pass).
- `cargo check -p storage` — pass.
- `cargo clippy -p storage --all-targets -- -D warnings` — pass.

The auth tests own many localhost fixtures. A default parallel run collided with other lanes'
simultaneous listener tests in the managed sandbox and hung its final callback timeout; the same
58-test binary passes deterministically with one test thread. No external OAuth account or network
service was used.

## Root integration sequence

For initial authorization or token rotation, the `app.rs` adapter must use this exact order:

1. Call `physical_secret_slots_for_reconciliation(4_096)` before accepting secret-backed work. For
   every row with `legacy_cleanup_username`, delete exactly `base`, `base.refresh`, and `base.dcr`
   from `secret::KEYRING_SERVICE`, then call `acknowledge_legacy_secret_source_deleted`. On any
   inventory/delete/ack error, remain fail-closed. If the row is also orphan, only then delete its
   physical bundle and call `acknowledge_physical_secret_slot_deleted`.
2. Resolve `credentials.keyring_username`. Parse a versioned username as `PhysicalSecretSlot`;
   leave a legacy username untouched until a successful migration.
3. Allocate `SecretBundleStagePlan(logical_id, previous_typed_slot)` and call
   `stage_oauth_token_bundle` for the new access/refresh/DCR bundle.
4. For a legacy logical pointer call `Db::publish_legacy_credential_secret_slot_cas`; for an already
   versioned pointer call `Db::rotate_credential_secret_slot`. A false legacy CAS result means the
   new physical slot is orphan and must never cause deletion of the legacy source.
5. If the database call definitely fails before commit, delete the new slot and return failure. If
   commit outcome is unknown, delete neither new nor legacy sources and reconcile after restart. If
   it commits, make success visible to refresh waiters, then delete the previous typed slot. A
   failed old-slot delete is reported and recovered by the next startup reconciliation; it must not
   roll the committed pointer back.
6. During bootstrap, enumerate every database-referenced versioned slot and call
   `reconcile_orphan_secret_slots` before Connector/proxy workers accept secret-backed work. Treat
   inventory/reconciliation failure as fail-closed. Legacy usernames are deleted only through the
   durable v30 marker sequence in step 1, never by prefix enumeration.

For typed refresh, allocate the plan before calling `refresh_access_token_for_slot`. Its callback
receives `(&OAuthToken, Option<&SecretString>)`; stage those token values plus the borrowed DCR,
commit the ST01 pointer transaction, clean the new slot on commit failure, and return `Ok(())`
immediately after commit. Delete the previous slot only after the refresh function returns
`Refreshed`, so cleanup failure cannot be shared to waiters as a false publish failure.

Every secret-backed external execution path must acquire one `RedactionLease` for all bundle
secrets before permission/audit execution reaches the transport, abort with external-call count
zero on acquisition error, and hold the lease through subprocess/session teardown and redactor
flush. PR-IN01/PR-AU01 must not call the legacy unchecked registration API for this path.

## Pre-IN01 diagnostic hardening

- `LogicalCredentialId`, `PhysicalSecretSlot`, `SecretBundleStagePlan`, and
  `StagedSecretBundle` now have fixed redacted `Debug` output. Secret-bearing bundle types remain
  non-Clone/non-Serialize and their ownership/accessor APIs are unchanged.
- Physical-slot parser sources and every `SecretStore` error crossing the public
  inspect/stage/read/delete/inventory/reconcile boundary are replaced by fixed low-cardinality
  codes. Rollback and deletion no longer retain logical IDs, UUIDs, keyring coordinates, secret
  input, or backend error chains.
- Hostile-store and unique-marker regressions cover every public bundle diagnostic surface,
  nested holders, rollback-incomplete paths, and the bounded diagnostic scanner.
- Root verification: `cargo test -p secret -- --test-threads=1` passed 59/59 plus doc-tests;
  all-target check, strict Clippy, package rustfmt, and scoped diff-check passed.
