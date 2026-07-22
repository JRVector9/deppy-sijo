# PR-IN01 atomic Connector cutover

## Auth refresh exchange prerequisite

- Added and re-exported `auth::exchange_refresh_token_once(Duration, SecretString,
  &RefreshParams) -> anyhow::Result<RefreshOutcome>`.
- The function performs one synchronous request through the existing redirect-disabled
  `BoundedOAuthHttpClient`; it retains the existing timeout and 1 MiB OAuth response ceiling.
- The API has no store, database, keyring, coordinator, or publish parameter. It does not retry;
  the caller owns single-flight coordination and physical-slot publication.
- Legacy and typed-slot refresh paths now call the same public primitive, so this prerequisite does
  not introduce a second production exchange path.
- Focused tests cover success, exact one-request accounting, timeout, provider/transport error
  sanitization, no retry, and oversized-response rejection.

## Physical-secret bootstrap prerequisite

- App bootstrap reconciles bounded Staging/Orphan rows, completes Published legacy-cleanup markers,
  and validates every live physical access slot before any runtime or secret-backed worker starts.
- Legacy logical access/refresh/DCR bundles migrate through register staging → complete bundle stage
  → pointer CAS → exact legacy deletion. Unknown commit outcomes remain durable and stop bootstrap;
  no cleanup guesses or automatic retries are performed.
- A final bounded inventory requires every credential to reference an owner-matching Published
  physical slot with no outstanding legacy marker. Focused complete-bundle migration and idempotent
  rerun pass 1/1.

## Atomic production cutover

- `app.rs` is the composition root for `AppConnectorRepositoryFactory`, the dedicated repository,
  physical-slot secrets adapter, event-only host wake, one lazy coordinator, and its latest-only
  snapshot reader. Construction does not open the Connector database or create a worker.
- Home, status, Connector settings, and Composer consume the same immutable snapshot. Composer
  retains one server ID and virtualizes at most 256 enabled servers plus one matching 256-item tool
  page; it no longer materializes or queries a flattened server × tool catalog.
- The 5,495-line mixed `ui/connectors.rs` implementation and its module export are deleted. There
  is no feature-flagged or fallback production path.
- Every legacy Connector DB/MCP/audit/secret exception was removed. `check-boundary` now reports
  zero explicit leaf exceptions and `check-deps` remains green across 23 crates.

## Bounded host and import lifecycle

- Connector import/OAuth/external-link work, Composer context-file selection, Settings workspace and
  project folder selection, and web-remote approval URLs execute through one app-owned lazy
  single-flight task.
  The task has one result slot, processes one host action, performs one event-only repaint, and is
  joined before another task starts. Unused state has no host task/channel/timer/poll. Folder
  results retain at most one 32 KiB path and backpressure later host work until the queue-1 Settings
  worker accepts the corresponding transaction.
- Composer Send shares one bounded Arc history snapshot with the app host; render performs no file
  write. Clipboard/image paste returns a continuation, is materialized by the same host task, and
  is reduced to a redacted-Debug payload capped at 16 paths/256 KiB before UI completion. History
  writes use a bounded JSONL temporary file plus rename and do not automatically retry failures.
- Import reads use the 1 MiB+1 pattern and the parser rejects a 257th map item before building
  candidates or report rows.
- Browser launcher children are waited/reaped on the background task. All new launcher errors are
  fixed and URL-free; dynamic OAuth inputs remain non-Clone, non-Serialize, and redacted.
- Canonical Slack projection treats duplicate rows as `Failed` with no selected server. The storage
  reconnect transaction re-enables one disabled canonical row atomically and rejects duplicates.

## Verification

- `cargo test -p auth one_shot_exchange --no-fail-fast -- --test-threads=1`: 5 passed.
- `cargo test -p auth --no-fail-fast -- --test-threads=1`: 101 passed; doc-tests passed.
- `cargo check -p auth --all-targets`: passed.
- `cargo clippy -p auth --all-targets -- -D warnings`: passed.
- `cargo fmt --package auth -- --check`: passed.
- `git diff --check -- crates/auth docs/build/PR-IN01-summary.md`: passed before this summary was
  added and rerun afterward.
- Connector host import byte bound: 1 passed.
- Connector host failure continuation: 1 passed.
- Slack projection: 2 passed.
- Bounded import parser: 10 passed.
- Composer snapshot/virtualization/context-file/clipboard/history continuation: 48 passed.
- App-host folder/URL/history bounds: 3 passed.
- Settings exact-path find-or-create adapter: 1 passed.
- Reaped browser launcher: 3 passed.
- `cargo check -p deppy-sijo --all-targets`: passed with zero warnings.
- `cargo clippy -p deppy-sijo --all-targets -- -D warnings`: passed.
- Workspace rustfmt check, `git diff --check`, zero-exception boundary gate, and dependency gate:
  passed.

The first focused run encountered the managed sandbox's transient localhost bind denial in all four
listener-backed cases. No product/test bypass was added; the subsequent full and focused reruns
executed those same cases successfully.
