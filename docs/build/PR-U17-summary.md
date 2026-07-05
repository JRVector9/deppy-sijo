# PR-U17 Build Summary

## Input Findings

- PR-R08 identified status detector cost as a performance backlog item.
- The current runtime already avoids hidden snapshots by using backend `screen_text()` rather than `TerminalViewportSnapshot`.
- The remaining low-risk gap was unnecessary screen text reads for detectors with no regex patterns.

## Scope

- Keep the existing three-stage status detector model.
- Add a pre-scan cost gate and lightweight stats.
- Do not change status semantics, UI, storage schema, or regex configuration UX.

## Changes

- Added `StatusDetector::should_scan_screen(produced_output)` so callers can avoid creating screen text when no regex patterns exist.
- Updated the runtime pump to call `screen_text()` only when the detector requests it.
- Added `StatusDetectorStats` with stream and screen scan counters.
- Kept idle heuristic evaluation independent from screen scan creation.

## Tests

- `cargo test -p session` - pass

## Acceptance Criteria Check

- [x] Waiting/running/done/error detection behavior remains covered by existing tests.
- [x] Hidden session snapshot ban remains intact; this path still uses `screen_text()`, not viewport snapshots.
- [x] Regex-empty sessions skip output-triggered screen scans.
- [x] Status detector cost is measurable through stats.
- [ ] Confidence score and user override are not implemented in this scoped PR.

## Regression Risks

- A detector with no regex patterns now ignores input-triggered screen scan requests. This is intentional because only idle heuristic remains active.
- Regex-enabled detectors still scan the tail screen text after output or input, preserving current behavior.

## Resource Impact

Reduces backend grid text reads for regex-empty sessions, especially hidden sessions that only need idle detection.

## Security Impact

No storage/logging/security behavior changes.

## I18n/CJK Impact

No terminal output translation changes. UTF-8 chunk-boundary detection tests still pass.

## Rollback Plan

Revert the `should_scan_screen` runtime call to the previous produced-output/request condition and remove the stats fields.

## Follow-up

- Add confidence scoring only when UI/notification semantics are ready.
- Add user override controls as part of a broader activity/status UX PR.
