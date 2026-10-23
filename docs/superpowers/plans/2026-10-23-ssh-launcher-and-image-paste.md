# SSH launcher and image paste implementation plan

**Goal:** Display installed SSH-account agents and paste local clipboard images into their remote terminals.

**Architecture:** Reuse remote-host service RPC and existing asynchronous app/workspace I/O ownership. Resolve a bounded server-side PATH without executing account startup scripts. For images, transfer into private remote-owned storage before emitting a remote pathname; never emit a Mac cache path into remote PTYs. User approved native hierarchy implementation after the HTML review; preserve existing async SSH/trust operations while replacing the flat picker.

**Tech Stack:** Rust workspace, OpenSSH transport, typed SSH RPC, egui operation/generation fences, standalone HTML.

## Phase1: Mockup (completed)
- [x] Read common popup rules/case01; produce interactive hierarchy HTML.
- [x] Open visible browser and test10 interactions plus desktop/390px overflow/footer geometry.

## Phase2: Agent discovery (implemented and tested)
- [x] Verify actual server PATH/install locations and running helper environment read-only.
- [x] Add failing tests in `crates/remote-host/src/services.rs` for Codex/Grok in home bins with a nonlogin inherited PATH.
- [x] Add `agent_environment` shared bounded search-path resolver and forward its PATH to spawned supervisor; do not mutate global environment or run shell init.
- [x] Run `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target cargo test --offline --locked -p deppy-remote-host`; verify an env-shebang child can resolve its interpreter using the same path.

## Phase3: Image clipboard (completed)
- [x] Trace workspace and composer clipboard completions plus SSH RPC request fencing and file size limits.
- [x] Write RED integration showing remote clipboard request currently rejected; ensure local text fallback and exact original session ownership are preserved.
- [x] Add only bounded image staging RPC/worker operations needed by the proven flow. Private account-owned staging; no arbitrary write path; no shell interpolation; bounded data/time/concurrency; explicit failure; stale/cancel must not paste.
- [x] Run relevant remote-host/runtime/app tests, fmt and targeted clippy, and review actual scoped diff.

## Phase4: Approved native hierarchy (completed)
- [x] User authorization: “새로 만든 시안대로 트리방식등 적용되면 돼”.
- [x] Execute RED for independently verified selection surviving navigation (`None` vs `/srv/selected`).
- [x] Apply lazy bounded tree, independent expand/select, breadcrumb/up/path edit, fixed inner scroll/full selected path; preserve generation/request identity fences.
- [x] Update popup case01/design and all five locales; execute focused state/UI tests and offscreen native renderer, compare to approved HTML.

## Parallel ownership
Three GPT-6.1-Sol/xhigh agents have disjoint scopes: discovery reviewer now native hierarchy/i18n/docs; server private cache module; client clipboard module. Root owns App/workspace glue, helper integration tests and all final Cargo checks. Source/test only; existing native process and live user sessions remain untouched.

## Executed evidence so far
- Root remote-host unit29PASS/2ignored and actual helper integration3PASS. Subsequent helper integration4PASS includes new discovery→supervisor→PTY home-bin/env-interpreter fixture under `/usr/bin:/bin` PATH.
- Initial cache quota Io failure traced to fork-inherited flock. Storage regression3RED→GREEN; deterministic close-only lock RED→explicit unlock GREEN; module8PASS.
- Client receipt tests6PASS/2FAIL before exact path validation and pre-mutation check; final10PASS. App clipboard28PASS/1ignored and SSH-workspace59PASS/3ignored before current native-tree changes.
- Final native21PASS/0FAIL/1separate render; actual arrow/Home/End/Enter/Space/Esc and painted-path geometry verified. Final8state renderer produced16PNGs; desktop/narrow visually reviewed.
- Final App3721unit+39integrationPASS; combined app/server/shared-popup all-target Clippy, fmt, boundary, i18nPASS. Existing project-key test startup Busy race fixed only in test; first failed full run preserved. Large clipboard fallback boxed after Clippy finding; no lint suppression.
- Linux ARM64 musl29unit+4actual helper integrationPASS as unprivileged nobody. Docker tmpfs exec correction was test-runner only.
- Final source/log/screenshot hashes and compressed evidence: `docs/qa/assets/ssh-hierarchy-20261023/`; QA report `docs/qa/2026-10-23-ssh-hierarchy-launcher-image-paste.md`. No live user CLI image turn claimed.

## Delivery boundary
- [x] Update `docs/CODEX_HANDOFF.md` with exact results and remaining live validation boundaries.
- Do not launch/restart the native app or stop remote user sessions. No package/release; version increase required when later delivering changed product build.
