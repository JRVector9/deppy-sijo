# Actual execution evidence

Canonical checkout: `/Users/jr/Desktop/projects/deppy-sijo-performance`, baseline `70380aea`.

- `cloud-agent.log`: `DEPPY_CLOUD_DESIGN_IMAGES=/Users/jr/Desktop/projects/deppy-sijo-performance/docs/qa/assets/cloud-settings-redesign-2026-10-13 CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo cloud_agent::` — 161 passed / 3 existing ignored.
- `shared-settings.log`: `CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo ui::settings::tests` — 37 passed.
- `i18n.log`: `CARGO_BUILD_JOBS=2 cargo test --locked -p i18n` — 8 passed, doc tests 0.
- `clippy.log`: `CARGO_BUILD_JOBS=2 cargo clippy --locked -p deppy-sijo -p i18n --all-targets -- -D warnings` — exit 0.
- `boundary.log`: `CARGO_BUILD_JOBS=2 cargo run --locked -p xtask -- check-boundary` — exit 0.
- `cargo fmt --all --check` and `git diff --check` actually executed, both exit 0; no stdout.
- Independent CLI review: `codex -c model='"gpt-6.1-sol"' -c model_reasoning_effort='"xhigh"' review --uncommitted` — exit 0. Final conclusion preserved; full investigation log remains `/private/tmp/deppy-cloud-settings-redesign-review.log`.
- Reviewer independently executed the final 44 UI tests and 8 i18n tests, formatting and diff checks. UI 44 are included in CloudAgent 161; do not add them twice.

The historical `geometry-navigation-red.log` filename in /private/tmp ended in `green` even though its actual result was **3 passed / 2 failed**. It is stored here under the correct RED label. The subsequent GREEN has **5 passed**.

Review-three RED: 5 passed / 3 failed (clipped long-detail actions, notification selection disappearing, latent actor input consent). Following production fixes, UI 40 passed. Later final UI 44 and CloudAgent 161 include all three regressions.

Narrow-row and answer-name search proofs each actually failed before their fixes, and are included in the final CloudAgent GREEN. Intermediate compile/fixture errors (WidgetInfo API, NodeT import and temporary label borrow) were corrected before final checks. These errors are not reported as behavior RED.

Rendering uses the production egui leaf, in-memory controller/real loopback server fixtures, and the actual native CJK font loader; screenshots are not HTML renders. No provider account, user DB, clipboard, running app or tunnel was modified. No release app was built or restarted.

Final source commit `2fd73a5b3c98a17e43a0e5ecfd6839e76db81c3a` (main feature `51b23890`). Full CloudAgent161 ran with the 80px columns and 34px hit areas; only checkbox header alignment and two test fixture service-state coordinates changed afterward. The affected final UI44 were rerun (`ui-final.log`), followed by final strict clippy/fmt/diff. No unchanged tests were added twice to the 206 total.

`ui-final.log`: `DEPPY_CLOUD_DESIGN_IMAGES=/Users/jr/Desktop/projects/deppy-sijo-performance/docs/qa/assets/cloud-settings-redesign-2026-10-13 CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo cloud_agent::ui::` — 44 passed.

Archived log content is unchanged except removal of empty trailing EOF lines, to satisfy `git diff --check`.
