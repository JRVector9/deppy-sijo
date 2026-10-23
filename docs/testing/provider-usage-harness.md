# Protected account usage

JR requested the initial correction and protection on 2026-10-23. The approved collector,
source selection and status-bar display are listed in `provider-usage-contract.json`.
This is a change guard and mandatory regression suite. It is not an OS file lock or a
security boundary against someone deliberately changing both the guard and its manifest.

## Contract

- Claude values are **percent used** for the current session/5-hour and all-model weekly
  windows. Model-specific Opus/Sonnet/Fable percentages never replace the all-model week.
  Missing values stay unknown and never borrow the next section's percentage.
- Claude reads its bounded regular snapshot file asynchronously. A complete reading under
  60 seconds avoids an unnecessary CLI launch. A partial or older file allows the worker
  to refresh, at most once per 60 seconds. Completed CLI results are drained even while a
  file read is pending. The newest available measurement wins separately for each window.
- Failed reads/probes retain successful readings with their **original** timestamps, up to
  10 minutes. Reset windows and older pre-reset probe values are invalidated. Future
  timestamps more than 60 seconds ahead are rejected; legacy files use their file mtime.
- The statusLine writer refreshes the measurement timestamp before deduplicating unchanged
  database values, and carries optional per-window reset times. A missing window is null.
- Grok values are **percent remaining**, with optional monthly quota and USD credits.
  The worker handles old/current trust prompts only in its own usage-probe PTY, separates
  text and Enter events, and tears down the child within its existing 25-second budget.
- OSC sequences end at the first BEL or ST; multiple controls never consume intervening
  quota text. Incomplete controls remain hidden until complete.
- Unknown/loading values keep their existing dash, and are never invented as zero. Disabled
  providers are hidden. The actual egui status-bar harness checks provider order, partial
  values and widths from 300 to 1200 points.

## Required verification

```sh
python3 scripts/verify-provider-usage.py
python3 -m unittest discover -s scripts/tests -p test_provider_usage_guard.py
```

Use `--offline` when Cargo dependencies are already cached. `CARGO_BUILD_JOBS` and
`CARGO_TARGET_DIR` may select normal local build resources. The verifier first checks
SHA256 of focused files and marked regions, then checks the exact test inventory,
rejects removed/ignored mandatory tests, and executes all required tests. It never
updates the approved hashes automatically. No live account, GUI restart or network
access is required by the behavioral harness.

`--check-only` checks approved sources without building. CI runs this and the guard's
mutation self-tests in Linux fast-checks; macOS also runs the executable contract. The
mutation tests alter only synthetic temporary files. Full workspace tests remain in CI.

## Intentional changes

Unrelated work must leave protected files/regions and gates unchanged. A change requires
an explicit user request to change usage behavior, a failing regression, a reviewed fix,
and the affected tests. Then, and only then, reseal with that authorization reference:

```sh
python3 scripts/verify-provider-usage.py --seal --authorization 'Explicit user request and date'
python3 scripts/verify-provider-usage.py
python3 -m unittest discover -s scripts/tests -p test_provider_usage_guard.py
```

If an authorized new test is added, update the exact mandatory inventory as part of that
review. Do not delete a test, add `#[ignore]`, change expectations, or reseal solely to
make verification pass. Record executed checks in `docs/CODEX_HANDOFF.md`. A changed
artifact delivered later must follow the repository version/release rules. These test
commands never grant permission to restart the running app.
