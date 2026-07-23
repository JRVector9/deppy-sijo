# PR-AU02 — Bounded Audit Retention

## Outcome

PR-AU02 makes finalized audit history bounded in production and composes retention atomically with
the existing authorization lifecycle. Retention is caller-driven at an existing audit write or
owner-recovery boundary. It creates no thread, timer, polling loop, periodic wake, foreground
`VACUUM`, network activity, or additional runtime.

`Prepared` remains durable and is never a deletion candidate. Crash/owner recovery first changes
the exact dead owner's `Prepared` rows to `Unknown`; only then may the same transaction prune those
now-finalized rows. A retention failure rolls back the lifecycle mutation, permission/audit
preflight, or outcome that invoked it.

The final hardening bounds retention *work*, not only retained state. A production lifecycle
transaction performs one indexed newest-first walk of at most 4,161 rows: the 4,096-row production
ceiling, one 64-row delete batch, and one sentinel. It can delete at most 64 whole rows and 8 MiB
of logical row data. If that one bounded pass cannot reach every hard ceiling, it returns the
opaque `AuditRetentionNormalizationRequired` marker and the caller rolls the lifecycle mutation
and attempted GC back together. The marker has static, content-free `Debug`/`Display`, and its
private field prevents downstream construction.

## Policy and deterministic ordering

`audit::AuditRetentionPolicy::production()` fixes the maximum policy. A custom policy may lower a
ceiling but cannot raise one.

| Finalized audit resource | Production ceiling |
| --- | ---: |
| Retained finalized rows | 4,096 |
| Retained finalized logical bytes | 8 MiB |
| Maximum finalized age | 30 days |

Finalized means `Succeeded`, `Failed`, `Unknown`, or `Denied`. `Prepared` rows are excluded from the
window and always preserved. Logical bytes include every variable-width audit column, with SQLite
text measured as UTF-8/blob bytes rather than character count.

Retention ranks whole finalized rows newest first by `COALESCE(completed_at, created_at)` and then
ID. It removes rows outside the age window, after item rank 4,096, or after the cumulative 8-MiB
budget. It never truncates a row or partially retains encrypted/redacted fields. The post-delete
bounded window proves all three ceilings before the caller may commit; the former whole-table rank,
delete, post-delete aggregate scan, and expired-row count are gone. SQLite page high-water space is
left reusable; foreground `VACUUM` is deliberately excluded.

Schema migration v31 adds a partial expression index over the exact finalized predicate and
newest-first order. Production queries name that index explicitly, and an `EXPLAIN QUERY PLAN`
regression proves no temporary sort. Creating the index is a one-time upgrade cost proportional to
the existing audit table; steady-state lifecycle work is fixed after migration.

## Legacy normalization and fail-closed admission

An existing database may begin above the hard ceilings by more than one foreground batch. All six
storage lifecycle entries first attempt their normal transaction and one retention window scan.
Only an `anyhow` downcast to the exact typed normalization marker rolls that whole attempt back,
runs the private storage completion loop, and retries the DB operation once. Error text is never a
retry predicate, so a hostile dependency returning the same display string cannot induce repeated
work. Each normalization iteration owns and releases a separate `IMMEDIATE` transaction after at
most 64 rows / 8 MiB and returns progress; incomplete-without-progress is a static error. Callers
cannot forget maintenance, while healthy steady state still pays exactly one indexed window rather
than an unconditional preflight scan plus the mutation scan.

No external-call grant exists during retry. The non-Clone `AuthorizationPlan` is borrowed while the
permission/audit row is issued into an opaque operation, and exact plan binding is validated into a
second opaque type before commit. Only after prune and commit is the plan consumed into the
infallible `AuthorizationPreflight`. `record_tool_audit` may repeat encryption on the rare rollback
path, but audit key creation uses one exact idempotent keyring ID; the regression proves two DB
attempts create the key only once and retain only the committed ciphertext.

The loop is lazy and performs no work, polling, thread creation, or repaint while audit APIs are
idle. A legacy individual row larger than the 8-MiB delete budget cannot be safely normalized under
the declared per-transaction byte ceiling; it returns the static
`audit_retention_row_exceeds_normalization_byte_limit` error for explicit offline repair rather
than silently performing unbounded work.

## API and ownership

The `audit` crate owns the policy, selection algorithm, and low-cardinality result:

- `AUDIT_RETENTION_MAX_FINALIZED_ITEMS`;
- `AUDIT_RETENTION_MAX_LOGICAL_BYTES`;
- `AUDIT_RETENTION_MAX_AGE_SECONDS`;
- `AUDIT_RETENTION_DELETE_BATCH_ITEMS` and
  `AUDIT_RETENTION_DELETE_BATCH_LOGICAL_BYTES`;
- `AuditRetentionPolicy`;
- `AuditRetentionNormalizationRequired`, opaque and externally non-constructible;
- `AuditPruneReport`;
- `AuditNormalizationReport`;
- `prune_audit_logs_in_transaction` for a caller-owned lifecycle transaction.
- `normalize_audit_retention_batch_in_transaction` for caller-owned legacy maintenance.

There is no public standalone prune function. The only standalone prune wrapper is crate-private
and compiled for tests; production external audit APIs require a caller-owned transaction. The
storage batch and completion loop are private implementation details reached automatically from the
six existing `Db` lifecycle entries.

The reports expose only item/byte counts and completion state. They have no operation, tool,
server, URL, input, token, error text, Prepared identifiers, or other high-cardinality field.

Optional encrypted-input failure also emits only static structured diagnostics:
`kind=audit`, `phase=encrypt_input`, and `error_code=secret_store_error`. The underlying
`SecretStore` error is never formatted or attached to the event. The durable audit row still
commits its redacted input with `input_encrypted_blob = NULL`.

The `storage` crate does not add a competing standalone retention API. It owns invocation through
the existing `Db` authorization boundary and calls `prune_audit_logs_in_transaction` with the
production policy and current Unix time at these transaction points:

1. after `record_tool_audit` inserts the record;
2. after `acquire_authorization_owner` performs dead-owner `Prepared` to `Unknown` recovery;
3. after normal authorization preflight is prepared and before commit;
4. after revision-CAS preflight only on the committed branch; the stale branch performs no prune;
5. after `complete_authorization_outcome` writes its terminal lifecycle;
6. after `close_authorization_owner` changes its remaining Prepared rows to `Unknown`.

Permission mutation plus audit preflight, lifecycle transition, and retention therefore either
commit together or all remain unchanged. A failed audit preflight still permits zero external tool
calls, and an `Unknown` operation remains non-retriable under the AU01 protocol.

## Verification

- `audit` all-target/all-feature suite — 64/64 tests passed.
- `cargo test -p storage --no-fail-fast` — 233/233 tests passed; doc-tests passed.
- Final integration-focused retention tests — production item `N+1` 1/1 and lifecycle/GC rollback
  failpoints 2/2 passed.
- Production item retention proves 4,096 finalized rows remain and the newest inserted row is
  retained.
- The legacy fixture starts 65 rows above the production ceiling and converges inside the same
  public write call through a 64-row batch, one-row batch, and one DB-only retry. A counting
  `SecretStore` proves the rollback/retry creates the audit encryption key exactly once.
- A separate 130-row expired fixture models the first write after more than 30 idle days and proves
  it converges in bounded batches without requiring repeated user actions.
- Source laws cover all six lifecycle entries and require one steady-state prune call, no eager
  normalization call, exactly one rare-path normalization loop, and at most one DB retry.
- A hostile non-retention error with the exact
  `audit_retention_normalization_required` display string performs one attempt and is returned
  unchanged; the source law requires typed downcast and rejects string comparison.
- The query-plan regression proves the bounded window uses
  `idx_tool_audit_retention_finalized` and never builds a temporary order-by tree.
- Completion failpoint proves a DELETE/GC failure leaves the operation `Prepared`; removing the
  failpoint allows the same completion to reach `Succeeded`.
- Recovery failpoint proves a DELETE/GC failure rolls `Unknown` recovery back to `Prepared` and
  releases the owner lock; the next acquisition succeeds and reaches `Unknown`.
- Audit and storage all-target checks, strict Clippy with `-D warnings`, rustfmt, and scoped
  diff-check passed in their respective lanes.
- Existing plaintext-secret, secret-like persistence, authorization ownership, idempotent outcome,
  and unknown-no-retry regressions remain green in the integrated storage suite.
- A thread-local capture subscriber injects a hostile secret-bearing `SecretStore` error and proves
  the emitted event contains only the three static fields and never the hostile marker; the stored
  encrypted blob remains NULL.

## Rejected and failed approaches

- A timer or retention worker was rejected because idle production must have zero audit polling,
  wakeups, and threads. Retention is paid only at an existing write/recovery boundary.
- Post-commit or best-effort pruning was rejected because it could publish a lifecycle transition
  while leaving retention unbounded. The caller-owned transaction API makes failure atomic.
- The initial AU02 implementation ranked the entire finalized table, deleted every excess row in
  one `IMMEDIATE` transaction, iterated every `RETURNING` row, and scanned the table again for
  postconditions. Independent review rejected that state-only bound. The fixed indexed window and
  explicit batch normalizer replace it; no batch limit was raised to hide legacy state.
- A public standalone prune entry was removed because it allowed retention to commit separately
  from a lifecycle mutation. The crate-private test helper is the only isolated wrapper.
- An unconditional storage normalization preflight was implemented briefly, then rejected because
  every healthy lifecycle write would scan the 4,161-row window twice. The final rollback-triggered
  lazy retry scans once in steady state and pays normalization only for legacy/aged overflow.
- String matching for the rare retry branch was rejected because a `SecretStore`, SQLite context,
  or other chained error could reuse the text and trigger unrelated normalization work. The final
  branch recognizes only the opaque typed marker by downcast.
- Direct pruning of `Prepared` was rejected because delivery may be unresolved. Recovery must first
  durably classify it as `Unknown`, and automatic retry remains forbidden.
- Foreground `VACUUM` was rejected because SQLite can reuse freed pages and synchronous compaction
  would add avoidable write latency and I/O.
- The first combined storage authoring run contained test-only SQLite count conversion errors from
  the concurrent ST02 additions. Those were corrected before the AU02 production, rollback, and
  full storage evidence was accepted; no product behavior was bypassed.
- The first post-review focused command used `--exact` without the Rust module path and selected
  zero storage tests. It was not accepted as evidence; corrected filters ran the intended tests.
- Independent review found the former encryption-failure warning formatted the complete dynamic
  `SecretStore` error. The warning now discards that value and emits only static low-cardinality
  fields; the hostile-marker capture regression prevents recurrence.

## Deterministic completion versus release measurement

AU02 is deterministically complete: policy ceilings, whole-row ordering, Prepared preservation,
same-transaction integration, retention-failure rollback, sanitized reporting, compilation, lint,
and audit/storage regressions are proved locally.

This is not the final hardware release approval. The 30-minute idle/discover/cancel RSS, thread,
socket, and queue slope run plus release Scenario A–E CPU/RSS/frame-p95 measurements remain PR-BG01
gates on the fully integrated production build. AU02 adds no idle resource of its own, but the final
measurement must still verify the complete application. External OAuth/account smoke also remains
manual and requires user approval; it was neither required nor performed for this milestone.
