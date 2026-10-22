# CC + SSH runtime integration — 2026-10-23

## Objective / authority

Integrate frozen CC `24260718` with SSH descendant `48db9bf4` (including local main `bc950e88`) in `/private/tmp/deppy-cc-ssh-integrate-20261023`, branch `fix/cc-ssh-compatibility-20261023`. Root will review and absorb the merged tree. The merge is deliberately uncommitted. Original/main and other dirty trees are untouched. Source version0.15.1 remains research/test-only: no release, distribution, app launch/restart/stop, paid provider turn or live user SSH operation.

Root is sole Cargo/build/test executor with jobs2. This agent handles source edits, individual rustfmt and lightweight Python verification. Results below distinguish executed checks from source analysis.

## Root cause and boundary

SSH replaces `WorkspaceRuntime.runtime` with `WorkspaceRuntimeClient::{Local,Remote,Offline}`. CC launch calls `remote_worker_control` and `remote_worker_wake`, which only the underlying local runtime supplied. A textual merge does not provide these APIs and granting a local fallback for an SSH root would turn a remote folder string into local filesystem/process authority.

The intended combined contract is local-only CC host authority: Local delegates worker control/wake; Remote/Offline explicitly reject worker control and expose no wake. CC launch, MAIN authorization/input, local verification, discovery/worktree preparation and this-computer server grants must check workspace ownership before performing local work. Existing SSH original terminal, files, Git, PWA and close acknowledgements remain on their established SSH paths.

## Merge resolution

- `crates/app/src/app.rs`: retained both CC observation-loss handling and SSH pending-close event observation in active and warm runtime drains.
- `crates/core/src/lib.rs`: retained `work_management` and `workspace_target`.
- Five `crates/i18n/locales/*/messages.txt`: retained both CC and SSH message groups.
- `docs/CODEX_HANDOFF.md`: retained both previous histories and prepended this integration phase.

## Regression-first work / actual checks

Added real wrapper assertions for Local control delegation, Remote/Offline typed denial/no wake, and actual isolated Offline plus production-framed connected-SSH App caller tests. Root executed the initial API RED: exit101 in73.19s with12 E0599 compile errors for missing wrapper control/wake and the new typed unsupported variant. This is an integration compilation RED, not a behavioral test result. The source API incompatibility predates this fix. Root's first locked preflight failed because three newly merged path-package lock entries still inherited0.13.0; root aligned only those entries to workspace0.15.1, changed no third-party dependencies, and repeated the actual API RED.

After that API RED, local-only wrapper delegation and Result-handling callsites were added. Caller execution/verification/discovery/UI fences were absent for the behavioral RED run. The first behavioral attempt exited101 in19.07s because a new test accessed child-private `Verifications.worker` (E0616); this is a fixture compilation error, not a behavioral RED. The assertion was corrected to use the existing public `busy()` method before the repeated behavioral run. Root's corrected actual behavioral RED completed **0PASS/4FAIL** in60.26s. Catalog discovery incorrectly set `catalog_requested`; snapshot incorrectly enabled local execution settings. The two caller cases encountered an invalid default SSH folder before any remote-specific fence (`folder must contain 1..=8192 UTF-8 bytes`), so they are not claimed as proof of local process execution. The final caller fixture now sets a valid absolute shared-spelling folder, Codex selection, allowed agent and scopes, and explicitly executes `policy.validate_config` before calling the real endpoints.

Root executed the combined-source SSH GREEN: **5PASS/0FAIL**,34.07s, `ssh-behavior-green.{json,log}` under root `docs/qa/evidence/product-review-fixes-20261023/`. This focused command compiled the actual App and runtime/web-remote/remote-host dependencies; it does not claim those dependencies' test suites executed. Source remains frozen for root's combined integration checks. `require_command_center_local_workspace` requires persisted Local ownership and an actual non-remote active/warm wrapper when present. It guards launch, catalog, durable input continuation, delayed identity lookup and final prompt queue, MAIN scope collection, local/manual verification and Prepare→Run revalidation, this-computer grant, and scoped CC mutations. SSH snapshots disable those capabilities with a plain user-facing reason; wrapper denial remains a typed machine error. No SSH local-runtime fallback or unwrap was added. Final regressions include a remote-wrapper/local-metadata mismatch case and final queue admission in addition to Offline/connected calls. Original SSH terminal/files/Git/PWA paths are unchanged.

Modified integration-specific product/test files: `crates/app/src/{app,workspace_runtime_client,command_center_host,command_center_launch,command_center_main_host,command_center_observe,command_center_server,command_center_verification}.rs` and `crates/runtime/src/protocol/ssh.rs`. The existing SSH merge contributes its own larger file set; merged feature history is not attributed to these focused fixes.

Executed lightweight check: `python3 -m unittest scripts.tests.test_prepare_remote_host scripts.tests.test_dev_build scripts.tests.test_ssh_packaging` → exit0, **18 tests PASS**,2.400s. These tests use disposable static ELF fixtures, mock Cargo and temporary build-only paths; no actual Cargo compilation, app/companion execution, Docker or real helper staging occurred.

Root command requested:

```sh
cd /private/tmp/deppy-cc-ssh-integrate-20261023
CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo command_center_ssh_ -- --nocapture --test-threads=1
```

Root's explicit target cache: `/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target`. Root owns the exact environment/log and will report the result before product source changes resume.

## Additional root review / regression phase

Root's independent MAIN reviewer identified CC Git action bypasses in `apply_command_center_git_action`: Finder reveal resolves the same-spelling local path; OpenWorktreeShell stages `SpawnShellAt`; InsertPath stages direct `WriteInput` outside the CC input gate. Four test-first actual handler cases now cover those effects on Offline plus production-connected SSH fixtures. A synthetic focused mux supplies the InsertPath target; no shell or real provider is spawned. Root executed the expanded actual RED: **5 existing PASS / 5 new FAIL**, exit101 in26.19s. All four Git cases observed a staged local host/controller action. Evidence is root `docs/qa/evidence/product-review-fixes-20261023/ssh-git-manual-red.{json,log}`.

A fifth new test submits a real isolated manual-evidence worker request under Local metadata, changes the pending workspace to SSH metadata, pumps the actual completion, and expects no Passed application or save. The actual RED applied Passed instead of preserving NotRun, proving that completion lacked current workspace ownership. After this RED, one current-ownership condition was added to ManualRecorded completion. The Git handler now checks current Local ownership before its three local effects and emits the existing SSH unsupported notice. Individual rustfmt and staged/unstaged diff checks exited0. Product source is frozen for root's final focused GREEN; that result is still pending.

Read-only Git path was traced: `request_git_panel_io_at` chooses the SSH target root; action-owner `route_ssh_host_action` routes GitPanel through `RemoteWorkspaceFiles.git_request` and `queue_ssh_rpc`, never the local Git worker for an SSH owner. The minimal handler guard covers only Finder/shell/direct-input actions, preserving remote Git reads and the existing original SSH UI.

## Wire and helper assets (source inspection, not rebuilt)

The combined source keeps shared runtime wire36 (`crates/runtime/src/protocol.rs`). SSH client and server hellos both use this shared constant; the independent SSH envelope stays4. Exact protocol equality rejects old runtime31 helpers at handshake. Existing SSH rejection test currently checks the immediately preceding version; an explicit wire36/legacy31 rejection regression has been added in this integration; its execution awaits root combined gates. No remaining hard-coded runtime31 source assertion was found in `crates`, `scripts` or `xtask`; numeric31 terminal sizes/session ids and the legacy-rejection matrix are unrelated.

The older SSH evidence manifest is frozen historical evidence and is not relabeled as combined-source validation. A helper pair built before CC integration is not a wire36 artifact. `prepare-remote-host.py` validates bounded immutable ELF architecture/version/digest/size bytes; `build-remote-host.sh` performs the actual two-architecture source build and Linux `--version` checks, then seals the pair. Raw developer executables resolve `target/Resources/remote-host`; `dev-build.sh` refreshes only a previously validated managed pair via `prepare-dev-resources.py`. Packaging stages helpers under the macOS bundle Resources and its signature.

Future root-only helper refresh from this combined source, without launching Deppy:

```sh
cd /private/tmp/deppy-cc-ssh-integrate-20261023
# Choose a NEW path: build script refuses to overwrite old sealed artifacts.
CARGO_BUILD_JOBS=2 scripts/build-remote-host.sh --output target/remote-host/0.15.1-cc-ssh-wire36
python3 scripts/prepare-remote-host.py --source target/remote-host/0.15.1-cc-ssh-wire36 --version 0.15.1
DEPPY_REMOTE_HOST_ARTIFACTS=target/remote-host/0.15.1-cc-ssh-wire36 CARGO_BUILD_JOBS=2 scripts/dev-build.sh
```

These commands have not been executed in this phase. They produce investigation assets only at0.15.1; delivery requires a release version above every shipped branch, matching app/bundle/helper versions, a newly sealed pair, and bundle verification. `scripts/dev-run.sh` also launches Deppy and remains prohibited without an explicit restart request. Helper installation on a live server retains the existing generation-scoped user consent; no live install has occurred here. Current host code offers the install flow only when the versioned helper command is missing (exit127). A stale helper with the same app version and old wire is rejected as `ProtocolMismatch`, without automatic overwrite/retry. A future correctly bumped release uses a new versioned server path and follows the existing explicit install consent. Test-only0.15.1 does not authorize replacing any installed server artifact.

## Remaining

Initial five-case App SSH GREEN is complete; expanded ten-case Git/manual GREEN is pending after the actual five-new-failure RED and minimal fixes. Receive root focused and combined full-workspace results (including wrapper/SSH wire regressions), correct any actual compile/test issue, and record final outcomes/logs. Root may use the combined full gate instead of repeating the proposed focused wrapper/wire/check commands. Lightweight helper packaging tests already passed; actual helper builds/seals remain root-owned and unexecuted in this phase. No artifact, physical-device, actual provider or live SSH end-to-end claim.

## Final root focused GREEN

Root executed `command_center_ssh_` after the two reviewed Git/manual guards: **10PASS/0FAIL**, exit0/12.46s. Exact metadata/log in canonical integration `docs/qa/evidence/product-review-fixes-20261023/ssh-final-green.{json,log}`; log SHA256 `3d0446ed669b6b7d645a08365dbb0df303021ed691adc6ec1525e225c943f80d`. Covers actual Offline/connected-framed SSH, stale metadata/runtime mismatch, catalog/UI/control admission, four Git side-effect paths, and pending manual evidence after target change. Whole integrated workspace gates remain root-owned and pending. No live server/helper artifact release claim.
