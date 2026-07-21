# PR-SC01 — Versioned Secret Slots and Bounded Redaction

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
  managed sandbox because 29 MCP HTTP fixtures are denied at local `TcpListener::bind` with
  `Operation not permitted`; the other 49 MCP tests pass before the command stops.
- Scoped rustfmt and `git diff --check` — pass.

The auth tests own many localhost fixtures. A default parallel run collided with other lanes'
simultaneous listener tests in the managed sandbox and hung its final callback timeout; the same
58-test binary passes deterministically with one test thread. No external OAuth account or network
service was used.

## Root integration sequence

For initial authorization or token rotation, the `app.rs` adapter must use this exact order:

1. Resolve `credentials.keyring_username`. Parse a versioned username as `PhysicalSecretSlot`;
   leave a legacy username untouched until a successful migration.
2. Allocate `SecretBundleStagePlan(logical_id, previous_typed_slot)` and call
   `stage_oauth_token_bundle` for the new access/refresh/DCR bundle.
3. Call `Db::rotate_credential_secret_slot(logical_id, new_slot, sanitized_oauth_json, hint)`.
4. If the database transaction fails, delete the new slot and return failure. If it commits, make
   success visible to refresh waiters, then delete the previous typed slot. A failed old-slot delete
   is reported and recovered by the next startup reconciliation; it must not roll the committed
   pointer back.
5. During bootstrap, enumerate every database-referenced versioned slot and call
   `reconcile_orphan_secret_slots` before Connector/proxy workers accept secret-backed work. Treat
   inventory/reconciliation failure as fail-closed. Legacy usernames are outside the versioned
   prefix and must not be deleted.

For typed refresh, allocate the plan before calling `refresh_access_token_for_slot`. Its callback
receives `(&OAuthToken, Option<&SecretString>)`; stage those token values plus the borrowed DCR,
commit the ST01 pointer transaction, clean the new slot on commit failure, and return `Ok(())`
immediately after commit. Delete the previous slot only after the refresh function returns
`Refreshed`, so cleanup failure cannot be shared to waiters as a false publish failure.

Every secret-backed external execution path must acquire one `RedactionLease` for all bundle
secrets before permission/audit execution reaches the transport, abort with external-call count
zero on acquisition error, and hold the lease through subprocess/session teardown and redactor
flush. PR-IN01/PR-AU01 must not call the legacy unchecked registration API for this path.
