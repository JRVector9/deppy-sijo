# Native correction evidence — 2026-10-20

Source: `1dd0ba7afc46a347b2340566b965304b33682f7d` on `fix/cc-review-native-20261020`. These are actual offscreen egui renders from the app Cargo test binary, with its real credential module. They are fixture renders, not captures of a running user app/session.

F03 and F08–F12 are covered by native interactions/visible widget geometry and visual inspection. F07 CHECK controls use the exact structured session target/request key and host capability, with foreign-workspace requests excluded. The parent supplies actual host approval projection/dispatch; the isolated build used an explicitly permitted, uncommitted empty pending-approval initializer in the host. This initializer is not in the code commits and is not evidence of host approval behavior.

Executed final checks:

- `CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo ui::command_center --bin deppy-sijo -- --test-threads=1`: 24 passed, 0 failed, 1 ignored, 3472 filtered. Log: `/private/tmp/deppy-cc-native-fix-20261020/cargo-final.log`.
- `DEPPY_CC_EVIDENCE_DIR=/private/tmp/deppy-cc-native-fix-20261020/screenshots-final CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo ui::command_center::tests::command_center_render_evidence --bin deppy-sijo -- --ignored --nocapture --test-threads=1`: 1 passed, 0 failed, 3496 filtered, 46.10s, 120 PNGs. Log: `/private/tmp/deppy-cc-native-fix-20261020/render-final.log`.
- Scoped `rustfmt --edition 2024 --check` on the seven changed UI files and `git diff --check`: passed. All six immutable approved reference hashes matched their manifest.

The render matrix covers 1440/1280/1000/760/390px, dark/light and font scale1/1.25, Main/Settings/Devices/empty/no-project states, plus supported worker/project drawers. The viewport wrapper removes the test harness's default CentralPanel margin, so `geometry.json` records the exact requested viewport. Fixture task titles, scopes, providers, devices, activities and timestamps map the approved HTML example.

Ten representative final PNGs are retained here. All120 states were inspected in seven contact sheets; twelve final full-size samples and the approved desktop/mobile reference were opened separately. Full private evidence and contact sheets: `/private/tmp/deppy-cc-native-fix-20261020/{screenshots-final,contacts-final}`.

Observed correction: assignment separators at421/483/545px give62px row pitch (previous109px); owner/device remain in a right column. Worker drawers start at app-header bottom48/46px, close shares the monitor header, and device management remains in the footer with many workers. Settings fields are constrained to620px and36px high at base scale (45px at1.25). All five mobile navigation labels remain visible beside rollback. Composer uses one border, a left target and right send button.

RED history is retained in private logs: missing VerificationFailed retry,109px assignments, clipped navigation, offscreen footer; a corrected Settings fixture demonstrated836px field width. Separate approval RED showed the missing displayed command. Visual review caught a composer send-alignment regression introduced during cleanup; its added geometry test failed before the correction. Initial Settings fixture/query failures and incompatible private rustc dependency artifacts were harness issues, not product bugs.

No app start/stop/restart, release build, version bump, provider/physical-device E2E, push or main merge occurred. `manifest.json` contains exact source and selected PNG SHA256 values; `geometry.json` contains all120 actual render geometry records and a pixel-derived assignment-pitch check.
