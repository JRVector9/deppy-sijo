# Approved settings/cloud preservation and App lifecycle fix — 2026-10-23

## Result and scope

The previously approved settings rules and cloud single-screen source is preserved on Command Center base `2426071872327faadfbb507d714d588989e0560e`, including all 27 required product/test paths. A separately authorized integration fix clears a displayed cloud token before Command Center can return from `App::ui`, while preserving the original cleanup after settings render. Root reproduced the defect with **0 passed/2 failed**, then verified the fix with **2 passed/0 failed** using actual isolated App frames.

Preservation worktree: `/private/tmp/deppy-settings-integrate-20261023`, branch `fix/settings-preserved-integration-20261023`. Read-only approved source: `/Users/jr/Desktop/projects/deppy-sijo-ime-cloud-20261015`, HEAD `f78ebb69c94ec7cb72ae9a1159d2137c0d6a5679`. Root executes Cargo in `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023`; this agent executed no Cargo command. No app launch/restart/stop, provider turn, release artifact, version change, commit or push was performed by this agent.

## Complete source provenance

[Original transfer manifest](assets/settings-preservation-20261023/transfer-manifest.json) remains frozen. It records the approved source, Command Center base and transferred SHA256 for every required path. Historical source manifests and unrelated source documents/designs were not copied or rewritten.

- **21 exact original copies:** the audit's 16 existing product files, `cloud_agent/ui/management_tests.rs`, `cloud_agent/ui/redesign_tests.rs`, and the previously untracked `cloud_agent/ui/one_screen.rs`, `cloud_agent/ui/one_screen_tests.rs`, `ui/settings/design_rule_tests.rs`. Each tracked source HEAD first matched its Command Center base bytes.
- **Scoped App transfer:** the original three-line post-settings disposal condition was inserted without copying the older entire `app.rs`. Its inverse projection reproduces all Command Center App bytes.
- **Five scoped locale transfers:** each source's 44-line append was inserted after the shared original prefix. Each Command Center locale's newer 108-line suffix remains byte-for-byte intact. Removing the insertion reproduces the complete Command Center locale.
- **Release files excluded:** `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md` retain the Command Center bytes and workspace version **0.15.1**. The original source's 0.13.1 release edits are excluded. This is unshipped source integration; future distribution requires a new version above all shipped branches.

[Final 28-path source manifest](assets/settings-preservation-20261023/final-source-manifest.json) lists the final source hashes explicitly. [Separate lifecycle overlay manifest](assets/settings-preservation-20261023/lifecycle-overlay-manifest.json) records the only follow-up changes: `app.rs`, a cfg(test)-only accessor in `cloud_agent.rs`, and actual App regressions in `keychain_startup_tests.rs`. Reversing its three-file overlay reproduces the original 27-path transfer exactly, plus unchanged Command Center bytes for the additional test file. Final own-worktree product/test inventory: **28 paths**; missing approved paths: **0**.

[Verifier](assets/settings-preservation-20261023/verify_transfer.py) validates all exact hashes, inverse scoped projections, full own-worktree product inventory, source preservation, protected release files and the separate lifecycle overlay. It does not rerun Rust tests. The inventory deliberately targets this dedicated worktree: root's combined integration includes other agents' approved changes and needs root's final source manifest.

```sh
cd /private/tmp/deppy-settings-integrate-20261023
python3 docs/qa/assets/settings-preservation-20261023/verify_transfer.py --check-source --with-lifecycle
git diff --check
```

The original source HEAD, complete `git status --porcelain=v1 -uall` digest and all 27 approved source hashes remain unchanged.

## Root cause and minimal correction

The approved post-settings disposal call alone was insufficient on the newer Command Center tree: `App::ui` calls `show_command_center` and returns before reaching settings rendering. Closing settings or changing category while that route is active therefore left the displayed secret owner populated.

The final App code uses one private `clear_inactive_cloud_connection_token` helper with the original predicate. It runs at the start of `App::ui`, before every UI return, and again at the original post-settings location. The late call retains same-pass disposal when the settings renderer closes or changes its category. The open CloudAgents category retains its newly displayed token. No permission or token issuance behavior was redesigned.

The test-only accessor exists solely under `cfg(test)` and is absent from the production API. Each new regression constructs actual `App` with temporary DB/config/sidecar and the existing mock Keychain guard, then calls `eframe::App::ui` through `Frame::_new_kittest`. It verifies open-CloudAgents retention, transitions via settings close or the actual category setter, stays in the Command Center branch, checks zero Keychain calls, and asserts disposal. It seeds only a synthetic display owner; no token is minted, server contacted, clipboard touched or provider process launched. The fixture shuts down and removes its own temporary files before evaluating the final assertions.

The `systematic-debugging` and `test-driven-development` skills were read before this follow-up. Test-only RED sources were supplied to root before the production fix was written. Exact increments are retained losslessly as [RED patch gzip](assets/settings-preservation-20261023/lifecycle-red.patch.gz) and [GREEN patch gzip](assets/settings-preservation-20261023/lifecycle-green.patch.gz). The GREEN patch applies only App changes over the original transfer and RED fixtures; root applied it without overwriting unrelated App edits.

## Actual execution evidence

| Executor and command/filter | Actual result | Evidence |
|---|---|---|
| Root, `-p deppy-sijo --bin deppy-sijo settings_token_lifecycle_ -- --nocapture --test-threads=1` before fix | 0 passed / 2 failed; exit101; 60.68s total | [RED JSON](assets/settings-preservation-20261023/lifecycle-red-execution.json), [exact log](assets/settings-preservation-20261023/lifecycle-red-execution.log.gz) |
| Root, same command after fix | 2 passed / 0 failed; exit0; 48.11s total | [GREEN JSON](assets/settings-preservation-20261023/lifecycle-green-execution.json), [exact log](assets/settings-preservation-20261023/lifecycle-green-execution.log.gz) |
| Root, `-p connector-ui -p i18n -- --test-threads=1` | connector-ui35 + i18n8 passed; exit0; 48.16s total; nested child1 not added again | [Execution JSON](assets/settings-preservation-20261023/connector-i18n-execution.json), [exact log](assets/settings-preservation-20261023/connector-i18n-execution.log.gz) |
| Root, explicit ignored/exact `ui::settings::design_rule_tests::design_rule_render_native_review_images` | 1 passed / 0 failed; exit0; 3.91s total | [Execution JSON](assets/settings-preservation-20261023/native-render-execution.json), [exact log](assets/settings-preservation-20261023/native-render-execution.log.gz) |
| This agent, transfer verifier before lifecycle extension; formatting22 original transferred Rust files; `git diff --check` | All exit0 | [Original lightweight checks](assets/settings-preservation-20261023/lightweight-checks.json) |
| This agent, final transfer+overlay verifier, three-file Rust formatting and diff checks | All exit0 | [Final lightweight checks](assets/settings-preservation-20261023/final-lightweight-checks.json) |

The agent independently read the canonical root execution JSON and logs and verified their hashes. Exact raw logs are retained as deterministic gzip; adjacent readable copies strip only trailing terminal whitespace. The verifier checks the original decompressed bytes against execution JSON digests. Both RED tests failed precisely at the final missing-disposal assertion; the positive retention, Command Center route and zero-Keychain controls had already passed. RED was not a compile failure. Root's first earlier stale-build attempt reused an old connector-ui artifact because copied source mtimes were retained; root refreshed imported source mtimes and obtained the meaningful RED above. The stale compile attempt is not counted as a failing product test.

The parent's earlier original-IME results (settings17 passed/1 ignored, cloud229 passed/3 ignored, separate ignored renderer1 passed) remain pre-integration evidence and are not added to the integrated totals here. All approved regression sources are present, including settings design18 (one ignored renderer), single-screen15, completion permission, OAuth inspection and latent input revocation tests. Root's broader integrated App/UI/cloud gates are recorded separately when executed. The explicit native renderer above generated fixtures under `/private/tmp/deppy-product-settings-images-20261023`; root reported inspecting General Dark narrow and Agents Light. This agent did not inspect those images. These mock-frame tests do not constitute a live app or physical-device capture.

## Failed transfer approaches and next commands

Locale EOF `git apply --check` failed because Command Center had already appended 108 lines. This was preflight before any file write; it was replaced with the proved common-prefix insertion. A short App `// T1:` anchor also failed preflight because it appeared twice; the unique complete existing comment replaced it. Neither was a product test failure or partial source application.

Product sources are frozen. Root owns final review, integrated gates and commits. Suggested remaining filters, only as required by the final combined diff, are `design_rule_`, `cloud_agent::`, broader `ui::`, and the ignored exact renderer `ui::settings::design_rule_tests::design_rule_render_native_review_images`; connector-ui/i18n already passed as recorded above. Never add overlapping filter counts as independent coverage. Root should import this QA report and its assets, reconcile the main handoff, and retain the separate original-transfer and lifecycle proofs. No original source docs or historical manifests should be replaced.

No release or restart is authorized by these checks. Do not run `scripts/dev-run.sh` as part of this integration.
