# SSH resume and terminal corrections plan

Goal: fix three reproduced defects, retain remote sessions through Mac sleep, retry exact existing sessions automatically, then make terminal file paths clickable.

Canonical tree `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023`, branch `fix/product-review-integration-20261023`, base a8eb51ae. User explicitly authorizes parallel implementation. No GUI restart or distributed release requested.

## Ownership and accepted design

- SSH agent owns remote-host supervisor/tests and runtime SSH protocol/client TTL. Actual stdout nonblocking writes + EOF cancellation +45s no-progress deadline. Default detached retention7days, maximum30days via existing env. No wire layout change. Tests use owned fixtures only.
- Renderer agent owns workspace snapshot install and row render cache. Invalidate skipped dirty lineage; preserve idle and adjacent-generation incremental behavior. Freeze and Home/Fleet regressions.
- Archive agent owns alacritty backend serialization. Conceal projects to spaces, excludes original combining text; preserve snapshot color/style/width/wrapped geometry and bounded writes.
- Root owns App retry state/integration. Previously connected exact workspace/target only,1–30s backoff, no input replay. Authentication/key/helper/expired lease block retry. Cancel/close/target change stop retry. Rotate attachment ownership and skip launcher on recovery.
- New path agent owns bounded pure path helper, workspace click routing/tests and shared document classifier visibility only. Existing deferred resolve/OpenPath, no render filesystem I/O. Root owns App document dispatch. Test first, implementation after root RED; no snapshot/wire extension.

## Verification steps

- [x] Root reproduced existing three bugs in preceding review.
- [x] Regression RED: TTL3FAIL, renderer1FAIL, App2FAIL, archive4FAIL. Initial bridge test compile error corrected, then actual2PASS1FAIL proves hang.
- [x] Agent source fixes complete. Root full terminal/remote-host/runtime/App GREEN done, including final added scope tests.
- [x] Root actual App auto-retry regression RED before implementation, then policy/security/identity tests and GREEN.
- [x] Path pure parser + actual click/drag/cold-cache tests RED then GREEN; exact async ownership and SSH/local path separation.
- [x] Cross-review diffs, full affected App/runtime/terminal/remote-host suites, strict Clippy/fmt/boundaries after coherent source settles.
- [x] Preserve evidence, update QA/handoff with actual results and limits. Physical sleep/real server network blackhole only claimed if executed.

Shared Cargo is root-only, serialized: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target cargo test --offline --locked ...`.
