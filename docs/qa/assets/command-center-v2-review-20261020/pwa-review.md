| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | web/shared/command-center.js:424 | A focused worktree checkbox throws on the next snapshot | Interrupts production snapshot rendering while editing settings | Restore text selection only on supported input types |
| medium | web/shared/command-center.js:238 | Snapshot replacement removes modal drawer focus | Escape stops closing an open project drawer | Preserve/recover drawer focus and its return control across snapshots |
| medium | web/shared/command-center.js:256 | Approved monitor filters/details/actions/rules/collapse are absent | Users cannot triage or operate workers from the persistent monitor | Restore approved monitor structure and authorized routes |
| medium | web/shared/command-center.js:414 | Main lacks the approved CHECK and LIVE sections | Attention items and recent progress disappear from the main workflow | Render attention summary and recent task-linked history |
| medium | web/shared/command-center.js:446 | Approved Terminal navigation is replaced by Settings | Top navigation no longer opens the selected actual terminal | Restore Terminal entry and keep settings with project context |

Read-only review of `/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020`, HEAD `9c0a820a7805c4f25a1a6674f3b657fff464eb06`; no product edits, Cargo, app launch/restart, release, push or merge. Current product source is the documented d2b4605c cohort. Review ground truth: approved v2 HTML, design spec, local source, and fresh isolated installed Chrome, rather than prior PASS summaries. Repository `docs/CODEX_HANDOFF.md` changed concurrently by the parent; I did not modify it.

## Reproducible findings

1. **Settings snapshot exception**, `web/shared/command-center.js:399,424` (runtime defect).
   - Trigger: navigate to Settings, focus `[data-cc-focus="setting-use_worktrees"]`, receive any `work_snapshot` with unchanged project/settings.
   - Expected: preserve checkbox focus and continue rendering. Actual: `selectionStart` exists on a checkbox and is null, so `[null,null]` is truthy; the code calls `setSelectionRange` on `type=checkbox`, throwing `InvalidStateError`. `render()` never reaches `updateComposer`, status synchronization, remote scroll restoration, or watch synchronization for that frame. Repeats while checkbox remains focused. The same defect reproduced through the real production concatenated bundle and app WebSocket callback, not only the shared fixture.
   - Evidence: `deppy-cc-review-pwa-evidence-20261020/results.json` → `findings.checkboxSnapshot`; `production-relay-results.json` → `production.checkboxErrors.captured`, including production stack `app.js:2279` mapping to shared line424.
   - Fix: capture and restore a numeric selection only on textarea and selection-capable text input types. Do not use an exclusion of only `number`. Add a regression using checkbox focus plus next snapshot.

2. **Drawer loses focus and Escape**, `web/shared/command-center.js:238,257,449,461` (runtime usability defect).
   - Trigger: at390px open project drawer (focus correctly moves to Close), then receive a snapshot before using the keyboard. `renderLeft` replaces the focused control; `renderRight` has the same replacement pattern.
   - Expected: focus stays in the active modal and Escape closes it. Actual Chrome: focus becomes BODY while `aria-modal=true` and drawer remains open. Real CDP Escape then leaves it open because `onKey` is attached to the now-unfocused outer element and the BODY key event never reaches it. Control case Escape before a snapshot closes it. A subsequent Tab does recover focus inside the drawer; the defect is the lost focus and immediately broken Escape, not a proven permanent Tab trap escape.
   - Evidence: `drawer-focus-results.json`: `escapeBeforeSnapshot=false` (closed), `afterSnapshot.active=BODY`, `dialog=true`, `escapeAfterSnapshot=true` (still open); screenshot `shared-drawer-escape-after-snapshot.png`.
   - Fix: preserve identity/focus of drawer controls or restore active drawer focus after rerender; refresh `drawerReturn` to a connected control so closing also restores focus.

3. **Monitor materially differs from approved design**, `web/shared/command-center.js:250-260` (approved UI omission, not an authority exception).
   - Trigger: same selected project containing implementation/running, review/result-awaiting-confirmation and verification/approval tasks, desktop1440/1280; open right drawer at1000/760/430/390.
   - Expected from spec11/19 and approved HTML: role+provider badge, scope, activity, device/time, All / Needs attention / Running filters, task/approval/result and exact terminal actions, execution-rules section, device-management footer, desktop collapse/expand.
   - Actual: only heading, whole-row task link, title/activity, provider/device/state/time, generic note. Role and scope are supplied by snapshot but omitted from compact rows. No filter, direct terminal/result/approval routes, rules/device footer, or desktop collapse control exists. Clicking a row always opens the generic work tab. This affects read-only inspection as well as authorized PWA actions; Relay's view-only policy does not require removing filters or metadata.
   - Evidence: `approved-1440-dark.png` vs `shared-1440-dark.png`, `relay-1440-dark.png`, plus source function above and raw DOM/control metrics in `results.json`. Exact goal/task titles/scopes/provider/device/activity came from the approved HTML's in-memory project objects, so the missing structure is not explained by empty data.
   - Fix: reproduce the approved monitor information/actions and persistent desktop toggle; route effects through current capabilities and retain read-only policy.

4. **MAIN omits CHECK/LIVE**, `web/shared/command-center.js:414-417` (approved UI omission).
   - Trigger: same project has a pending result, pending approval and nonempty task-linked history.
   - Expected from approved HTML and spec10/16: MAIN assignment/progress, CHECK attention count and exact task links, LIVE recent progress and all-history route. Native implementation also contains these sections at `crates/app/src/ui/command_center/main.rs:255-299`.
   - Actual: generic intro, goal, plain summary, duplicated task table and plan editor; no attention summary or recent-history stream is rendered at any width. History exists only on a separate top-level tab. At390 this pushes the third task beneath the fixed composer while the approved compact role summary fits all three; scrolling works, but the intended information hierarchy is missing.
   - Evidence: same-content desktop/mobile pairs above; `approved-390-dark.png` vs `shared-390-dark.png`; `results.json` contains content dimensions and the exact mapped input is `shared-logical-snapshot.json`.
   - Fix: render role-based MAIN assignments, current attention and recent history without duplicating the full tasks screen. Keep task/result/approval distinctions intact.

5. **Five-entry navigation test validates the wrong five entries**, `web/shared/command-center.js:446`, `web/shared/tests/command-center.browser.mjs:24` (approved navigation omission/test gap).
   - Expected spec12 / approved HTML: Main agent, Tasks, Terminal, Agents/devices, History. Actual shared/PWA/Relay: Main, Tasks, Devices, History, Settings. Terminal is absent; the head action `터미널로` calls `setOpen(false)` and returns to the original dashboard, rather than opening the current exact terminal. Terminal remains reachable by a session in the left drawer or a task detail button, so this is a navigation/parity defect rather than total loss of terminal access.
   - Evidence: approved vs shared screenshot pairs; `results.json`/`production-relay-results.json` `nav` arrays. Existing browser test checks only count5, so this mismatch passes.
   - Fix: validate semantic nav destinations and actual route behavior, including exact current session and unavailable-session explanation; place project settings in the approved project context.

## Parity checks actually executed

| Area | Fresh evidence/result |
| --- | --- |
| Matched-content comparison | Loaded approved HTML project/task/event objects, mapped IDs/native state names into shared snapshot; goal/tasks/scopes/providers/devices/activity preserved. Reference has fixed px font and no font-scale setting, so no claim of pixel identity or reference1.25 scaling. |
| Widths/themes | Approved reference12 unique width/theme combinations and shared24 unique width/theme/font combinations:1440/1280/1000/760/430/390 × dark/light; shared font1/1.25. `results.json` has48 records because each fixed-font reference sample was recorded twice. |
| Overall geometry | No document/app horizontal overflow in any sampled configuration. Desktop220/312 sidebars at1440/1280; right drawer <=1050, both drawers <=760 work. At1000 shared left width220 vs approved196, a smaller density difference recorded, not promoted to a separate issue. |
| Main composer | Focus, selection2 and `한글 초안` survive a normal snapshot. Layout viewport resize390×420 keeps composer bottom383. Existing IME code inspected: composition guard, NFC,50ms commit fence, Enter vs Shift+Enter. I did not rerun the historical106 assertions or claim a physical IME keyboard test. |
| Touch targets | Measured: mobile menu32×34, send height34, top utility buttons height28. These are small, but approved mockup also contains similarly small controls; no new explicit44px approved requirement was found, so no separate regression filed. |
| Drawers | Fresh CDP open→Escape control, open→snapshot→BODY focus→Escape failure; Tab after snapshot recovers inside. Drawer exclusivity verified in source and sampled DOM. |
| Production PWA | Exact static_srv concatenation order and actual production index/app.js, isolated fake WS, matched snapshot;390 full viewport/no overflow, input enabled, checkbox error repeats. |
| Production Relay | Actual production Relay modules and WebCrypto fixture, encrypted matched-content projection; six widths full viewport/no body overflow, input disabled despite offered capabilities; continued original fixture to fresh `RELAY_SHELL_OK`. No Rust signature verification was rerun. |
| Safe area / virtual keyboard | CSS bottom `env(safe-area-inset-bottom)` inspected. Visual-only keyboard simulation layout844/visual420 leaves composer695..807, host844; it does not follow visualViewport. This is a reproducible geometry limitation to investigate, **not physical-phone verification or a confirmed physical mobile bug**. Top/side notch behavior was not validated on hardware. |

## Scope differences and boundaries

- Relay is explicitly view-only, and its denial of input/approval effects is expected. Fresh encrypted fixture confirmed it cannot gain input from offered capabilities; remote task observation remains View-only.
- PWA cross-TLS remote worker input is explicitly documented read-only in `docs/command-center-v2.md`; do not report that as a new regression. Native scoped remote input is separate.
- Production publication deliberately sets create-main/create-worker/close-worker capabilities false (`command_center_host.rs:1653-1663`), and PWA transport excludes those effects. Their absent buttons are current implementation scope limitations, not accidental JS authorization leaks. This does mean full native/PWA flow parity needs a separately explicit scope decision; source/handoff limits are not evidence that the original approved HTML was amended.
- Add-project/device setup/revoke and native assignment choices are not implemented as PWA effect routes; existing-workspace selection and capability-limited task inspection are present. This report does not turn each native-only operation into another duplicate finding.
- Actual host snapshots leave task approval at default and `Intent::Approval` always rejects. I sent this to parent for cross-surface/native integration ownership; browser fixtures fabricate approval controls, so fixture PASS does not prove real approvals work. Avoid duplicating the same host root cause in two reviewers' issue lists.
- No real provider, actual user database, physical Mac-to-Mac or phone, app restart, product package or version verification was performed.

## Commands and artifacts

All scripts and evidence are outside repository:

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-command-center-20261020
node web/shared/tests/command-center.browser.mjs --serve
# Printed visual fixture origin http://127.0.0.1:61303; temporary fixture server stopped during review cleanup.
node /private/tmp/deppy-cc-review-pwa-run-20261020.mjs
node /private/tmp/deppy-cc-review-pwa-drawer-20261020.mjs
node /private/tmp/deppy-cc-review-pwa-production-20261020.mjs
```

All final scripts returned exit0; the first creates48 raw measurements, the drawer script proves the failed interaction, and production script records the error as expected evidence then finishes the real Relay fixture. Primary evidence directory: `/private/tmp/deppy-cc-review-pwa-evidence-20261020/` (JSON, approved/shared/production/Relay PNGs). Screenshots actually opened for visual inspection: approved/shared1440-dark, approved/shared390-dark. Additional captures exist for each width dark plus390-light and production/Relay.

Failed harness attempts retained separately: the initial production script stopped on the intentionally reproduced uncaught checkbox exception; after catching it for evidence, a permissive custom asset fallback accidentally served Relay's service worker on the temporary PWA origin and intercepted a later navigation. This was a private harness routing problem, not a product defect. Final isolated run explicitly returned404 for `/sw.js`, used a fresh origin/profile, and exited0 with complete JSON and `RELAY_SHELL_OK`.
