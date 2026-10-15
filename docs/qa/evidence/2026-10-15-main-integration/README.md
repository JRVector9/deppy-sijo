# Main integration evidence (2026-10-15)

All files in this directory belong to the integration task. Earlier dated evidence elsewhere remains historical.

- `open-prs-before.json`: the 11 individually classified open pull requests.
- `local-branches-before.json`, `local-branch-decisions.json`, `local-branch-decisions.md`: 120 local branch heads, divergence and decisions.
- `worktrees-before.txt`, `worktree-dirty-before.json`: original 12 worktrees, tracked diff SHA256 and file counts. Raw duplicate dirty patches are deliberately omitted; originals remain in place.
- `original-preservation.json`: actual later verification of unchanged original tracked diffs.
- `catalog-red.log`: actual pre-implementation Claude resolved-model display regression (exit101, one failed test after successful compilation).
- `workspace-tests.log`: current 0.12.2 workspace run; exit0, 5733 passed/0 failed/65 ignored over 78 Cargo harnesses. See `workspace-result.json`.
- `relay-node-tests.log`: Node relay tests (16 passed, exit0).
- `viewer-readability.log`, `viewer-history.log`, `viewer-reading.log`: isolated Chromium contracts (114/89/182 assertions, each exit0).
- `cloud-results-mobile.log`: isolated Chromium cloud results contract (77 assertions, exit0).

Native application restart, live provider accounts, public tunnel end-to-end and physical mobile-device verification are not performed by this task. A local development package is distinct from a notarized distribution release.

`other-results.json` records the actual formatter/Node/Chromium commands and exits. `final-gates.json` is written by the sequential strict lint and architecture checks. The two DER files under `crates/web-remote/tests/fixtures/relay-loopback/` are synthetic localhost test credentials, never account credentials or production trust.

Raw archived patches contain mandatory unified-diff context prefixes, and original logs retain their exact trailing spaces/newlines. They are preserved byte-for-byte. `git diff --cached --check` for product files and for documentation excluding the two raw evidence directories passed; raw archival whitespace findings are not product defects and were not rewritten.
