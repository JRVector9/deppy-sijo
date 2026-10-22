# MAIN registration lifecycle correction — 2026-10-23

## Current objective

Reclaim retired Command Center MAIN registration slots, registrations, and operation replay records without weakening active/pending bounds, bearer revocation, installed runtime reuse, or one-time mutation admission.

## Investigation and design

- Worktree: `/private/tmp/deppy-main-lifecycle-fix-20261023`, branch `fix/main-registration-lifecycle-20261023`, base `24260718`.
- `MainTools::reconcile` revokes obsolete credentials and empties their slot but retains every tuple, registration tombstone, and operation record forever. Consequently the 32/128/256 bounds become lifetime limits.
- Actual pre-existing RED: `docs/qa/evidence/main-merge-additional-20261023/cc-main-retired-slots-regression.{patch,json,log}` in the SSH workspace. Root independently ran this regression: after 32 retired runtime/config tuples, active=0, slots=32, retired=32 and preparation33 fails with `main_registration_capacity_no_effect` (exit101). This agent has not run Cargo.
- Installed `Templates` owns a `MainRegistration` Arc and reuses it for later MAIN launches under the same runtime/config. Removing every empty retired slot would detach this legitimate launcher and break relaunch. The host supplies both its current config and exact installed revision for each resident runtime (at most two tuples per runtime). A failed save can restore an older durable config before any new runtime installation; retaining its exact installed slot preserves that legitimate retry. A replacement installation or runtime retires the former tuple. Obsolete external Arc clones remain disconnected if a tuple is later recreated.
- Retire authority through Auth before reclaiming registrations or their principal-scoped operation records. Auth removes the credential fingerprint; queued requests then fail both current-access and MAIN scope checks even if their former principal metadata is retained by a caller.
- Active principal operation records remain intact, including cached unknown/error outcomes and conflict detection. Retired principal operations are unreachable and can be reclaimed. Durable host launch IDs remain the source of launch identity reuse prevention.

## Modified files / status

- `crates/app/src/command_center_main_tools.rs`: copied the independently proven32-tuple RED; added same installed tuple129 relaunch, retired operations256 plus live replay/conflict and queued stale-principal checks, active/pending capacity checks, and obsolete Arc ABA coverage. `invoke` delegates real HTTP queue acquisition to a test helper.
- Production correction reclaims retired registrations and principal-scoped operations only after Auth revocation, while retaining live replay records. Slot reclamation retains current/installed runtime/configuration tuples supplied by `command_center_main_host.rs`.
- `crates/app/src/command_center_launch.rs`: one root-approved test reuses the actual installed `Templates::prepare` across129 retired MAIN lifetimes. No production launch/grant behavior changed.
- Added obsolete-slot Arc ABA coverage: an unconsumed old credential is cleared, the old Arc is detached, and recreating the same tuple cannot let that Arc consume the new pending credential.
- Host tuple collection uses runtime/configuration identity. A temporary `loaded=false` or storage-blocked project retains its installed slot while normal host authority validation retires its credentials. One host regression exercises denial, retirement, recovery, and single-use relaunch through the old installed Arc. Another uses the actual `finish_project_load` reducer to restore an older durable config after a failed new-config save, then verifies installed-slot reuse and old-slot detachment after a replacement installation.
- The129-registration regression also proves that the earliest retired bearer remains rejected after Auth evicts old connection metadata from its65-entry registry.
- Root confirmed the three integrated Rust source files are byte-identical to this worker. Source remains frozen for root integration and final checks.
- This QA note and the task-specific section of `docs/CODEX_HANDOFF.md` are owned by this agent. No commits, restart, provider call, release build or distribution.

## Verification actually executed

- `rustfmt --edition 2024 crates/app/src/command_center_main_tools.rs`: exit0.
- `git diff --check`: exit0.
- Root executed the new pre-fix regression filter: `command_center_main_reclaims_retired_`, **0PASS/3FAIL**, exit101,77.64s. All failures matched the intended lifetime causes: tuple33 denied, same installed runtime relaunch129 denied by registration history, and a new principal denied because retired operations still occupied256 records. Evidence in root integration: `docs/qa/evidence/product-review-fixes-20261023/main-lifecycle-red.{json,log}`.
- First root GREEN attempt failed at compile time (E0505 in the added Templates test: moving the operation string while borrowing it for activation), exit101/5.94s. No test ran. Corrected the fixture field to `operation.clone()`. Evidence: root integration `main-lifecycle-green.log`.
- After correction, `rustfmt --edition 2024` on all three owned Rust files and `git diff --check`: exit0.
- Root then executed the final combined-source MAIN filter: **28PASS/0FAIL**, exit0/36.98s. The filter includes installed durable-config recovery, actual `Templates`129 reuse,32-tuple reclamation, registration129 reclamation, retired-operation256 reclamation, live replay/conflict preservation, queued stale-principal rejection, obsolete Arc ABA, pending credential safety and32 active/pending capacity. Root evidence: `docs/qa/evidence/product-review-fixes-20261023/main-lifecycle-green-integrated.{json,log}`. Root confirmed candidate source byte identity. This worker independently read the root JSON:28passed/0failed/0ignored/3629filtered, log SHA256 `edea2d30aeb8b391ea009aa955b9ec4e9e67501a0b2c204d1a68829ca673007d`.
- This worker ran no Cargo/test/build command. Rust test results above were executed by root under the coordinated jobs2/shared-target gate; no broad/full suite or app release is claimed by this note.

## Scope boundary

- The existing32 slot bound still counts retained installed runtime/configuration slots, including idle slots. A synthetic caller retaining32 current installed tuples will still deny tuple33. This fix removes obsolete historical tuples; it does not redefine that concurrent installed-slot policy as an active-authority-only count. Normal App residency is separately constrained by `PerformanceConfig` (`max_warm<=8`, `max_live_warm<=12`) and bounded cross-workspace protection; no normal32-idle-installed residency path was found.
- Retired `MainTools` launch-ID tombstones are reclaimed. Production launch ID reuse remains rejected by the durable host draft gate (`launch:{id}`), before `MainTools::prepare`. Active registration IDs and all live-principal mutation IDs still reject reuse/conflict. Retired principals cannot authenticate or admit queued requests after their bearer fingerprint is removed.

## Executed root GREEN command

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-integration-20261023
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020/target cargo test --offline --locked -p deppy-sijo --bin deppy-sijo command_center_main_ -- --nocapture --test-threads=1
```

## Failed approaches / remaining work

- Rejected during design, not implemented: remove every retired slot. This would break legitimate existing `Templates` reuse on the same installed tuple.
- Additional rejected design filters: temporary project storage blocking must not imply slot obsolescence; current-config-only retention must not discard a different revision still installed in the same runtime before a failed save reloads its durable predecessor. Authority revocation stays separate from cache lifetime. Both cases have integrated passing regressions.
- Remaining for root: integrate documentation, finish independent review and required combined checks. No worker implementation remains. No commit, app restart, provider turn, versioned release build or distribution was performed.

## Read-only cross-review requested by root

- Reviewed the sibling SSH Command Center guard diff without edits or Cargo. The CC auxiliary Git action handler was missing the Remote/Offline fence used by the original Git handler: Finder reveal could stat/open Mac paths, and path insertion could bypass the disabled CC input capability through a direct runtime action. Reported as one cause/location to root; root forwarded it to the SSH owner. No further confirmed guard omission outside that handler was found. This is an SSH-owner correction and is not included in the MAIN28PASS claim.


## Exact next-agent commands

```sh
cd /private/tmp/deppy-main-lifecycle-fix-20261023
git status --short
git diff --check
git diff -- crates/app/src/command_center_main_tools.rs crates/app/src/command_center_main_host.rs crates/app/src/command_center_launch.rs
cat docs/qa/2026-10-23-main-lifecycle-fix.md
cat /Users/jr/Desktop/projects/deppy-sijo-integration-20261023/docs/qa/evidence/product-review-fixes-20261023/main-lifecycle-green-integrated.json
```

Source is frozen and root owns integration. Do not run Cargo independently or copy the whole sibling files over SSH changes; root merges the isolated hunks.
