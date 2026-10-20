| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | crates/app/src/ui/command_center/main.rs:500 | VerificationFailed hides otherwise-authorized CreateWorker | Remote verification failure has no native redispatch action | Render from host capability or include VerificationFailed |
| medium | crates/app/src/ui/command_center/main.rs:208 | Assignment outline loses owner column and nearly doubles row height | Fewer assignments/checks fit; ownership harder to scan | Restore compact number/title-scope/owner grid |
| medium | crates/app/src/ui/command_center/layout.rs:126 | Worker drawer starts72–78px too low plus standalone Close row | Less worker content fits on narrow screens | Start at header bottom; integrate Close in monitor header |
| medium | crates/app/src/ui/command_center/workers.rs:352 | Device management footer scrolls away with workers | Approved persistent device entry disappears | Reserve footer outside worker scroll area |
| medium | crates/app/src/ui/command_center/mod.rs:1068 | Mobile nav clips History with invisible overflow | Five-tab navigation becomes undiscoverable | Compact mobile tabs/rollback or show overflow affordance |
| medium | crates/app/src/ui/command_center/settings.rs:26 | Settings fields exceed620px cap and use20px text inputs | Form hierarchy drifts; phone text targets shrink | Explicit form bounds and36px input sizing |

## Scope and evidence

READ-ONLY native design parity review on branch feat/agent-command-center-v2-20261020, HEAD 9c0a820a7805c4f25a1a6674f3b657fff464eb06, product test source d2b4605c. No repository edits, Cargo, app launch/restart, release, push or child agents. Read repository AGENTS.md and top handoff. Applied design-review visual inspection discipline; the task explicitly excludes its fix/commit workflow.

Approved baseline: docs/mockups/multi-agent-command-center-v2-2026-10-20.html, docs/superpowers/specs/2026-10-20-command-center-v2-design.md and reference.json. Independently computed all 6 approved HTML/spec/PNG SHA256 values and obtained PASS for all 6. Opened all 3 approved PNGs via view_image.

Native screenshot root N = /private/tmp/deppy-command-center-native-postfreeze-evidence-20261020. Opened all 10: desktop-dark, desktop-light, medium(1280/font1.25), workers-drawer(1000), narrow(760/font1.25), mobile(390), mobile-projects(390/font1.25/light), mobile-workers(390/font1.25), devices-desktop, devices-mobile. Screenshot dimensions were inspected using sips. Pixel separator positions were measured with Pillow without modifying images.

Derived approved HTML evidence H = /private/tmp/deppy-cc-native-review-design-evidence-20261020. Isolated agbrowse/CDP profile /private/tmp/deppy-cc-native-review-browser, port9445. The local approved file was rendered read-only; desktop app height884 matches the native harness interior height884. Browser outer window minimum width500 prevents direct390 viewport, so the approved built-in phone preview was used (actual .app width390, height844). These are derived HTML references, not native evidence. Original approved PNGs remained unchanged.

## Details

### F1. Keep retry available after verification failure

Location: main.rs:500–506. Trigger: a remote task whose last attempt/result is VerificationFailed, with confirmed mixed plan, live grant/capacity and other host guards allowing retry.

Expected: reducer::can_dispatch deliberately accepts VerificationFailed (work_management/reducer.rs:363–383), and host projects create_worker capability accordingly (command_center_host.rs:773–785). Actual: native Work tab adds another whitelist of PlanReview/Queued/Failed/Cancelled and excludes VerificationFailed. Since host eligible_sessions excludes every remote task (host.rs:745 task.execution_device.is_none()), remote dispatch is also disabled (764–767). Thus an otherwise retryable remote task has neither existing-session Dispatch nor New worker action in the native Work tab. Local tasks also cannot create a replacement worker in this state, although an existing eligible local session may remain available.

Impact: after a genuine failed verification, users cannot start the corrective remote attempt from the task screen. Fix: trust the host-provided create_worker capability (which retains all admission checks), or include VerificationFailed in the UI state set consistently. Add native intent coverage for both enabled/disabled capability on VerificationFailed.

Evidence is exact source control flow, independently traced; no new runtime/native functional test was executed by this reviewer. Parent requested this functional candidate explicitly and confirmed no extra screenshot was needed.

### F2. Restore the compact assignment outline and owner column

Location: main.rs:208–223, global item spacing/rule helpers mod.rs:721,776–785. Trigger: home view with three existing tasks, every supported width.

Approved HTML:33 specifies .outline-row as number/minmax title-scope/104px owner columns (85px under1250;76px mobile), with owner border-left and12px vertical padding. DOM measurement of unchanged approved HTML at1440 gives62.09375px first/second rows; phone390 gives60.53125px. The provider and device stay in the right column on phone as well.

Actual native: provider/device are a third stacked line inside the title vertical. Desktop/native mobile separators are y421/530/639 and449/558/667, exactly109px per row.1280/font1.25 separators445/565/686 give120–121px. Just the first two rows cost~94px extra at font1, and the third row is cut by viewport fading in desktop/mobile evidence. CHECK and LIVE exist in source (main.rs:253–309), so they are pushed below scroll, not omitted. The approved desktop same-height derived screenshot already exposes CHECK heading; native does not. This also changes the approved main-task hierarchy, not merely font rasterization.

Impact: users scan fewer assignments and cannot compare owner/device in a stable column; attention entry is pushed farther down.

Evidence: N/desktop-dark.png, N/desktop-light.png, N/medium.png, N/mobile.png; H/approved-html-desktop-1440-app884.png, H/approved-html-phone390-light.png. Fix: render a responsive three-column grid; separate row padding from generic rule() spacing and use approved12px/10px title/scope sizes at base scale.

### F3. Use the approved full-height worker drawer

Location: layout.rs:126 (right_drawer min y = projectbar.bottom()), mod.rs:1025–1032 (standalone Close before monitor).

Trigger: width1000/760/390 and right drawer open. Approved HTML:35 absolutely positions the worker drawer top0 within .layout, beginning immediately after app header. Measured1000 approved drawer app-relative y48, height812. Native1000 drawer starts app-relativey126,78px lower; mobile beginsy118 instead of46,72px lower. The native standalone34/37.5px Close control plus item spacing adds another row, whereas approved Close shares the48px monitor header.

Impact: a narrow screen loses roughly116–122px of usable monitor height; supplied mobile-workers screenshot only reaches the third worker header. Fix: anchor drawer to header.bottom and put title/count/Close in a single header row. Preserve project overlay/backdrop input blocking.

Evidence: N/workers-drawer.png, N/mobile-workers.png; H/approved-html-workers-drawer1000-light.png. Left drawer already begins at header.bottom and does not have the same anchor defect.

### F4. Keep device management in a persistent monitor footer

Location: workers.rs:352–365 inside the ScrollArea begun251–254.

Trigger: three workers, or a smaller viewport/large font. Approved HTML:33,103 has .monitor-foot as flex:none sibling of .monitor-scroll, displaying device count and device management at the panel bottom. Native places manage_devices after every worker and rule in the same scrolling body and has no fixed footer/count. It is not visible at bottom in any supplied worker screenshot, including desktop1440. Rules themselves exist at325–350 and may scroll in the approved design too; the actionable issue is the separate persistent footer that is missing structurally.

Impact: the intended always-available device-management entry vanishes as the worker list grows. Fix: split monitor header/list/footer and reserve bottom footer before allocating worker ScrollArea, using current device data/capability rather than fixture count.

Evidence: N/desktop-dark.png,N/desktop-light.png,N/workers-drawer.png,N/mobile-workers.png; H/approved-html-desktop-1440-app884.png,H/approved-html-workers-drawer1000-light.png.

### F5. Preserve discoverable mobile primary navigation

Location: mod.rs:1068–1096, AlwaysHidden horizontal scrollbar and110px*font_scale reserved action width; base Button font13 atmod.rs:726.

Trigger:390px. Approved HTML:36 uses11px mobile tab text and keeps all5 labels visible; measured built-in390 phone nav width273.40625px, right edge app-relative323.40625px. Native default390 mobile screenshot completely omits History from the visible strip, while font1.25 mobile drawers clip Devices too. Rollback remains visible but the strip gives no overflow cue. Source retains horizontal scrolling, so this is discoverability/clipping, not a claim that History is impossible to access.

Impact: users miss a primary destination; the approved five-tab information architecture no longer reads as complete on phone. Fix: fit mobile11px typography and compact controls, or provide an explicit overflow control/scroll indicator while keeping rollback reachable. Add a visible-label test rather than merely outer-header/rollback containment.

Evidence: N/mobile.png,N/mobile-projects.png,N/mobile-workers.png; original approved mobile-main.png and H/approved-html-phone390-light.png.

### F6. Apply the actual Settings form geometry

Location: settings.rs:26–31; field()/popup::field uses default TextEdit dimensions. Trigger: Settings desktop1440 and390, plus760/font1.25.

Approved HTML:10 caps .form at620px and gives .field input/.field select36px height. Native settings.rs:26 expresses620px intent, but the rendered desktop fields spanx256–1092=836px. The max-width is set on an already allocated full-width UI, and child controls request desired_width(INFINITY), so it does not produce the approved form column. At390 the Project name singleline input is y333–353=20px and folder y398–419=21px; at desktop they are similarly20px, versus36px approved. Select fields remain34px and large notice creates another inconsistent scale.760/font1.25 still shows this mismatch.

Impact: a wide worksheet replaces the constrained form column, while name/folder have unusually shallow touch targets on phone despite tall surrounding controls. Fix: allocate an actual min(available,620px) form child/scope and explicitly size singleline fields to36px (scale sensibly with font) rather than depending on default TextEdit height.

Evidence: /private/tmp/deppy-cc-native-design-review-20261020/screenshots/settings-desktop.png, settings-mobile.png, settings-narrow-125.png; H/approved-html-settings-phone390-light.png. Source-only session/scope fields have legitimate runtime reasons; their presence is not treated as a design bug.

## Other inspected differences, not promoted to core findings

Worker cards visibly lose the approved role badge and opposite-edge status/time/terminal alignment (workers.rs:276–321). They are~231px versus approved203px for short first/second cards, separate from the severe assignment-outline inflation. This is visual drift, but the six core findings above have clearer behavior/layout consequences.

Devices runningserver fixture renders expanded endpoint/fingerprint/project-grant configuration before list/Connect (devices.rs:150, server.rs:29–120); the approved screen is device-overview-first. TLS/project trust needs are real and provider support differs, so this review does not assert missing supported devices from fixture differences. Revisit information hierarchy with the actual connection workflow, separately from the definite persistent-footer finding.

Composer has an inset second editor border, shorter editor, centered target and no desktop keyboard hint versus approved single surface (main.rs:846–913). Exact target and IME implementation is outside this visual review; this lower-impact presentation difference is not a release-blocking finding here.

## Parity checklist / boundaries

- PASS visually/source: dark/light core palette matches approved RGB; wide three-region structure; breakpoint1050 worker drawer and760 project drawer; projectbar heights84/78/72 and header48/46; main and selected worker targets; project/path/settings stay above workspace; separate body and monitor scrolling; mutually exclusive drawers/backdrop; sidebar task targeting/selected styling structure; approved references immutable.
- CORE DEVIATIONS: assignment grid/density; right drawer anchor/header; fixed device footer; mobile visible navigation; Settings geometry. Native retry state mismatch is a separate functional finding.
- EXTRA native evidence: opened all7 parent-produced PNGs under /private/tmp/deppy-cc-native-design-review-20261020/screenshots: Settings desktop/mobile/760font1.25, empty-project desktop/mobile, no-project desktop/mobile. Empty project and no-project show clear instructions and Add project entry; no additional blocker found. Parent reports actual offscreen1PASS (17private tests filtered), using byte-identical UI sources and only private appended test; a private unused SensitiveInput shim means no secret-handling behavior claim. This reviewer did not execute that harness.
- NOT failures: fixture project names/content; all-Codex vs mixed providers; zero vs3 devices; browser chrome/native window controls; safety-driven unsupported-provider/permission boundaries; CHECK/LIVE/rules merely below scroll rather than absent.
- No actual user-running app inspected. No interactive native harness rerun or new Cargo performed by this reviewer. Existing screenshot cohort is native egui fixture rendering, not user DB/session evidence. Derived HTML snapshots were opened and DOM-measured; no internet consulted or required.

## Review execution record

Executed: rg/source reads; AGENTS/handoff inspection; git status/diff read-only inspection; SHA256 six-reference verification; sips image dimensions; view_image all3 approved + all10 supplied native + all7 additional native + derived HTML desktop/Settings/Devices/drawer references; Pillow divider measurement; isolated agbrowse local-file navigation/snapshot/DOM measurements/screenshots. No product/test source edits, no Cargo or test suite by this reviewer. The initially resized browser has minimum500px window width, corrected by the approved built-in390px phone preview; one stale ref clicked Theme before corrected snapshot/click, producing light derived references only. No native state was touched.
