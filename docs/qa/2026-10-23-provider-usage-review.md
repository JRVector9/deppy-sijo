| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | crates/app/src/app.rs:38974 | Stale or partial Claude file data suppresses a fresh CLI query | Old weekly value persists and the 5h slot disappears | Separate refresh freshness from quota-window expiry; merge slots by measurement time |
| medium | crates/app/src/grok_usage.rs:419 | OSC stripping greedily consumes visible text between ST-terminated sequences | Usable output and startup prompts become empty; same code exists in Claude | Correct OSC boundaries for BEL and ST and add mixed/chunked fixtures |
| medium | crates/app/src/grok_usage.rs:356 | Trust recognition matches only the older “trust this folder” text | Installed Grok 1.0.67 waits at its trust prompt until probe timeout | Recognize the current startup state; restrict any automatic confirmation to the owned probe directory |
| medium | crates/app/src/claude_usage.rs:254 | Percent search crosses the next usage section | A missing 5h value incorrectly takes the weekly percentage | End search at the next section label and preserve the missing slot |

# Claude / Grok usage investigation — 2026-10-23

Investigation complete. **Four confirmed product defects; no product fix or release was applied in this review.** Canonical tree `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023`, branch `fix/product-review-integration-20261023`, baseHEAD `a8eb51ae5d8cdaaff2b9246ad96aeb474d724ed1` plus previously documented uncommitted changes. Current source0.18.0; running app is still0.16.0 PID31144. All three usage modules and the relevant Claude selection block are identical to running0.16.0 source commit `1c8ce64ea576b0e41b6c8f255d648da37f0c7075`; this is not explained away by the older running version. All1082 product inputs match the prior rebuild exactly. No GUI stop/launch/restart, source modification, commit/push or user-server change.

## 1. Claude selects stale partial cache instead of querying

Before diagnostic CLI execution, `~/.deppy-sijo/claude-usage.json` contained `{five_hour:8, seven_day:79, updated_at:1792734982}`; its age was approximately6h. The read-only app accessibility status showed Claude weekly79%, with no5h slot. `fresh_usage_windows` (`app.rs:8611` vicinity) accepts5h for300minutes and weekly for10080minutes. The reader therefore returns `(None, Some(79))`. In `app.rs:38974`, `.or_else(...)` short-circuits on that partial `Some`, so the CLI is never asked for the missing5h or newer weekly value. Even a fully populated file can override newer CLI data until these long window lifetimes expire.

The original, unmodified `fetch_claude_usage` executed against installed Claude Code2.1.309 returned **5h23% used, weekly84% used** in7.462s. This proves actual drift, not a hypothetical cache concern. Final read showed the file still8/79, so the diagnostic did not repair the cache. Source of snapshot age and refresh admission must be represented separately from validity of the rolling quota window; a weekly window is not a seven-day refresh interval. Preserve timestamps/reset times and refresh/merge missing or older slots without relabeling old data as live. `read_snapshot` also treats missing timestamps as age0, an older-cache compatibility rule that belongs in the same freshness change.

The official [Claude status-line schema](https://code.claude.com/docs/en/statusline#available-data) exposes used_percentage and resets_at separately. Its [update rules](https://code.claude.com/docs/en/statusline#when-the-status-line-updates) are event-driven with an optional refresh timer; the mere presence of a file is not evidence of current measurement. Avoid undocumented credential/API scraping to compensate for a local selection bug.

## 2. ANSI/OSC cleanup deletes visible content

Both `grok_usage.rs:419` and `claude_usage.rs:274` use `\x1b\][^\x07]*(?:\x07|\x1b\\)`. The greedy body allows an earlier ST (`ESC \`) inside the match, so it extends to a later terminator and removes the visible text between two independent OSC sequences.

Actual installed Grok output:5876bytes,4OSC starts,3BEL endings and1ST ending. Original stripping leaves **0 text characters**. A diagnostic control using shortest-terminated OSC matching on the exact same buffered bytes preserves4805characters and reveals the current trust prompt. This control still produces no quota data because defect3 remains; changing the regex alone is not a complete Grok repair.

Deterministic parser fixtures use the actual original strip+parse functions: visible `Weekly limit 30% used` between two ST-ended title controls returnsNone, while the same visible panel with BEL-ended controls returns70%remaining. Claude similarly returnsNone withST and12/34 withBEL. Full raw terminal data was kept only in the probe process memory, not persisted; evidence records counts and redacted label diagnostics. A bounded terminal-control scanner or precisely bounded OSC parser should cover both terminators and mixed/control-fragment cases.

## 3. Grok startup trust wording changed

Installed Grok1.0.67 shows **“Do you trust the contents of this directory?”** for its owned `~/.deppy-sijo/usage-probe` directory. The collector recognizes only `lower.contains("trust this folder")`. After resolving the text-stripping problem, this different phrase would still be missed. The original live probe was executed once plus two instrumented observations: each ended at about25.29s withOk(None), not an authentication/API failure established by evidence.

A separate fake CLI uses a real PTY and calls the original `run_grok_usage_probe` with a6s budget. It has no user credentials/settings and cannot run tasks. Only prompt wording differs:

- Current wording: only `/usage` sent, no trust response, resultNone after6.388s.
- Prior wording: `/usage`, confirmationEnter, then `/usage` sent; returns70%remaining after4.459s.

This isolates the recognizer independently of OSC parsing and network timing. No real Grok trust prompt was approved by this review. A repair must avoid accepting arbitrary unrelated prompts; confirmation scope should stay restricted to the probe's own known directory. Use startup-state readiness rather than assuming every CLI is ready exactly2s after spawn.

`current()` collapses errors/empty results with `.ok().flatten()`, retains the previous successful Grok value for10minutes, then returnsNone; the UI renders that as`—`. Existing tests confirm that expiry. It explains why a broken collection path can first appear normal and later become a dash; there is no per-failure reason in the current cache/log path. Do not fix this by showing old numbers indefinitely or converting missing values to0. The official [Grok status-line documentation](https://docs.x.ai/build/features/status-line) documents context/cost fields, not a guaranteed subscription-quota payload, so those values are not a quota replacement.

## 4. Claude percentage search crosses sections

Actual parser input `Current session\nLoading usage data...\nCurrent week (all models)\n34% used` returns `(Some(34), Some(34))`. The5h result must beNone. `extract_percent_after_label` searches12following lines without stopping at the next usage label. This is an additional reproducible mismeasurement under partial rendering; it was not proven to cause the specific79% UI observation. The baseline full panel12/34 parses correctly. Bare percentages without an orientation returnNone, but this was not observed in installed Claude output and is **not** filed as a confirmed compatibility defect.

## Verification / evidence

Executed on this review:

- Original source included directly in standalone Rust harnesses, linked to actual Cargo-resolved app dependency rlibs; both harnesses and instrumented observer compiled. Non-fetch UI freshness/DetectedAgent/path wrappers are diagnostic stubs, not an App UI test. Parser and fetch/protocol functions are original code; instrumentation is confined to an evidence copy that records output metrics without changing collector behavior. No tracked product source edits.
- Original live Claude and Grok probes and two additional Grok output observations as listed above; each owns and tears down its own PTY. Existing agent terminals receive no input.
- Real-PTY fake modern/legacy prompt comparison and OSC/section-boundary fixtures. Results demonstrate failures in current product behavior; no fixed build/E2E recovery is claimed.
- `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target cargo test --offline --locked -p deppy-sijo --bin deppy-sijo usage -- --test-threads=4`: **60PASS,0FAIL,5ignored** in0.05s tests /1.286s command. Ignored tests were not claimed as executed. Passing existing tests miss the above cases; full App suite was not rerun for a review-only task.
- Product-source SHA256 comparison all1082inputsPASS; original running-version usage-module and selection-block provenancePASS; `git diff --check`PASS. Exact outputs and hashes under [evidence](assets/provider-usage-review-20261023/).

## Suggested repair scope

1. Shared control stripping plus Grok startup recognition and bounded protocol regressions. Verify actual current CLI after both causes are corrected; no arbitrary extra timeout or real-session input.
2. Claude source freshness/admission and per-window merging, including reset/timestamp handling. Regression for a6h-old partial file versus a newer CLI result; prevent stale week data starving5h.
3. Claude section-boundary parsing; preserve unknown values, and report redacted collection status separately from quota values so a timeout, missing support and uninitialized state do not look identical.

These are proposed corrections, not completed code changes. Current artifact remains0.18.0; no new release/rebuild/restart in this review.
