# Command Center v2 PWA fix evidence

2026-10-20. Review fixtures rendered from product commits `995493fefad29925016bc2ce3617b8f6ea676a9f` and `e93ab90297c19b6974459b94bceff2a3e4f02d97`. This set covers F04/F05/F13/F14/F15. It does not capture a running user app, user database, a real provider, or a physical phone.

## What changed

- Settings snapshots preserve checkbox focus without using unsupported text selection. Both modal drawers preserve the exact focused control, Escape handling and a connected return control across updates.
- The worker monitor restores All / Needs attention / Running, provider and role badges, scope, activity, device and observed time, exact task/result/approval/terminal routes, execution rules, a fixed device footer and desktop collapse. Long monitor scroll survives snapshots.
- MAIN uses three compact role assignments; CHECK opens the exact task and displays actual host-owned session approval requests; LIVE shows the latest three actual history entries and links to full history. Advanced manual-plan input follows these sections; active plan review remains before CHECK.
- Five semantic navigation destinations are Main agent / Tasks / Terminal / Agents and devices / History. Terminal opens the selected exact task terminal or the exact bound main session. Unavailable identity produces an explanation. Project settings stay in project context; the separate × control folds the Command Center and preserves dashboard/reopen access.

## Captures

The approved HTML is `docs/mockups/multi-agent-command-center-v2-2026-10-20.html`. Its review toolbar and outer margins were removed only in the capture page so the application fills the viewport. Product CSS and application content were not rewritten. Titles, goals, roles, scopes, providers, devices, activity and history originate in its in-memory project objects. Observed times map to the matching task's actual approved history entry; no replacement time was invented. Local terminal UUIDs and capabilities are isolated transport fixtures, not evidence of a user session or real approval service.

| Files | Comparison |
| --- | --- |
| `approved-main-1440-{dark,light}.png`, `shared-main-1440-{dark,light}.png` | Same-content desktop main, base font |
| `approved-main-390-{dark,light}.png`, `shared-main-390-{dark,light}.png` | Same-content mobile main, base font |
| `approved-monitor-1280-dark.png`, `shared-monitor-1280-dark.png` | Desktop monitor; at 1280px it is inline, rather than a drawer |
| `approved-drawer-760-light-font1.png`, `shared-drawer-760-light-font1.25.png` | Narrow monitor drawer; the approved reference has a fixed font and no scale setting |
| `production-main-390-dark-font1.25.png`, `relay-main-1440-light-font1.25.png` | Actual production asset concatenation and actual encrypted Relay fixture |

The main summary uses the supplied DTO summary; its text and wrapping may differ from the reference's static explanatory paragraph. The central content scrolls independently above the fixed composer. These captures demonstrate structure and real routes in fixtures; they do not establish pixel identity.

## Executed checks

- `node web/shared/tests/command-center.browser.mjs`: **150 checks passed**, including meaningful checkbox/drawer RED→GREEN, exact approval owner and result revision, draft/IME/RPC fences, selected task/main terminal routing, unavailable exact identity, desktop collapse, long-list fixed footer and snapshot scroll, and Relay read-only guards.
- Production concatenation uses the actual `index.html`, shared viewer/results/Command Center assets and production `app.js`/`app.css` in `static_srv` order. Full production fixture: **COMMAND_CENTER_SHELL_OK:16**. Actual WebSocket callback checkbox updates and real CDP Escape/return-focus passed.
- Actual Relay modules plus the existing WebCrypto handshake/channel fixture: **RELAY_SHELL_OK**. The offered input capability remains disabled; only allowed observation requests are used.
- Matched shared, production and Relay matrices: widths **1440 / 1280 / 1000 / 760 / 430 / 390**, dark/light, font scale1/1.25. All sampled document/app widths fit the viewport. All five shared nav entries remain visible. Reference samples are fixed-font scale1; duplicate captures were deduplicated in `matched-matrix.json`.
- Node syntax checks for shared JS/browser fixture and production/Relay JS fixtures, plus `git diff --check`: passed.

`browser-results.json` records the actual 150-check report, production/Relay markers, uncaught JavaScript/console warning/error events and production window errors. `matched-matrix.json` and `production-relay-matrices.json` retain the measured geometry and route/input metadata without repeated button inventories. `matched-snapshot.json` is the exact review projection. `manifest.json` records source and artifact SHA-256 values.

## Reproduction and limits

```sh
node web/shared/tests/command-center.browser.mjs
node web/shared/tests/command-center.browser.mjs --serve
node /private/tmp/deppy-cc-fix-pwa-visual-20261020.mjs
node /private/tmp/deppy-cc-fix-pwa-production-20261020.mjs
```

The two private capture harnesses use an isolated installed Chrome profile and temporary loopback origins; their final logs are `/private/tmp/deppy-cc-fix-pwa-{visual,production}-20261020.log`. The fixture server origin printed by `--serve` must match the visual harness origin before rerunning it. The production harness creates its own origin. This durable set contains no requirement to rerun Cargo or launch Deppy.

Relay remains view-only. PWA cross-TLS worker input and unsupported provider/PTY command approvals retain their existing boundaries. Real host approval ownership, revocation, worker writes and normalized persisted history require root's separate backend gates; frontend fixture success alone does not prove them. No app stop/start/restart, real provider invocation, release build, version delivery, push or merge was performed.
