# PR11 — File-tree multi-selection

## Objective/source

Authorized Shift+mouse selection must carry every selected operation root through copy/move/delete. Isolated `/private/tmp/deppy-audit-pr11-wavefinal-followups-20261004`, baseline `9c7af797a4db4786b0dcc1187980e7ba71ead1de`. Root owns global handoff, independent CLI, integration/release/journal. Scope file_tree, App file-operation workers, and strictly file-DND Workspace adapters; PR10 owns keyboard/protocol blocks. No native app/user clipboard/data/PTY, version/lock changes or subagents.

Systematic-debugging/TDD/workstep guidance applies. Source investigation: marquee stores a BTreeSet, but built-in row drag and its explicit row override both use PathBuf. Header/row drop feeds one Move source. Copy/delete menu/keyboard already collect selections, but only delete excludes selected descendants. Clipboard paste always copies; host copy validates trees before effects but does not preflight all destination conflicts/cycles.

## Actual reproduction

`/tmp/deppy-pr11-gesture-red3-20261004.log` exited101: actual egui RawInput Shift marquee selected3, selected-name drag froze only1 host source. Changing selection after drag start left the original primary only, expected original3. Failure is a behavior assertion, not compilation. Preliminary attempts red/red2 failed to compile because egui0.36.1 modifiers are Event::ModifiersChanged and kittest's modifiers helper is private; corrected using the actual event API. These two are not RED proof.

Host all-member preflight/folder+child RED actually completed0passed/2failed in `/tmp/deppy-pr11-host-red-20261004.log`; both behavioral failures are detailed below. Source froze during every Cargo batch through `/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py`.

## Planned interfaces (initial plan; final result below)

Bounded frozen group payload with original tree root/generation; common operation-root ancestor/dedup policy for drag/copy/move/delete. Existing16 roots/32KiB per path/256KiB aggregate limits reject the whole request visibly. Option drag copies/default moves; same-folder move is no-op and same-folder copy preserves existing conflict/no-overwrite semantics. Cmd+Option+V carries move mode to bounded host clipboard reads. Typed release checks payload type before take, across tree header/row and terminal/attached consumers. Host whole-group preflight uses existing recursive copy/rename/remove engines, not duplicate native IO in UI.

The initial paragraph described inherited limits; the final shared limits below supersede it.

## Additional actual RED

All corresponding raw logs retained in `/tmp/`, exit101:

| Log | Behavioral failure |
| --- | --- |
| `deppy-pr11-host-red-20261004.log` |0/2: last-member conflict copied earlier files before failing; folder+child copied the child twice. |
| `deppy-pr11-typed-red-20261004.log` |0/1: actual tree header took unrelated SessionRow payload during failed PathBuf downcast, observed before end-pass cleanup. |
| `deppy-pr11-large-group-red2-20261004.log` |0/1:64 selected files refused by inherited16-root bound. |
| `deppy-pr11-secondary-red2-20261004.log` |Trash native callback received alias parent rather than canonical parent validated for group. |
| `deppy-pr11-final-safety-red-20261004.log` |Last copy source's canonical symlink target contained destination; first member copied before rejection. |
| `deppy-pr11-focus-red-20261004.log` |Terminal-owned keyboard focus with pointer over tree queued destructive Option+Cmd+V. |
| `deppy-pr11-trash-continuation-red-20261004.log` |0/1: after actual first private Trash effect, redirected parent symlink let next queued deletion remove private outside-root file. |
| `deppy-pr11-parent-component-red-20261004.log` |0/1: fake native clipboard folder plus folder/../a sibling produced1 destination root instead of2 because lexical descendant filtering omitted the sibling. |

Preliminary gesture and large-group attempts had egui API/shadowing compilation errors. `secondary-red` incorrectly included an extra `cargo` token in gate arrays. Focus fixtures initially lacked AccessKit nodes; real registered focus nodes corrected them. These are not behavioral RED proof. The initial `repeat=true` event lacked prior keydown and egui normalized it into a first press; that failure is also excluded as held-repeat RED. Final proof uses real prior key state, settles initial intent, expires debounce, then sends held repeat and release.

The first expanded gate had16pass/3fixture failures: parallel default/Option wrappers shared a fixed private directory; an empty later kittest frame overwrote event-preservation state; the repeat fixture lacked prior physical key state. Distinct fixture paths and event-specific/key-state observations fixed them without weakening focus or no-repeat assertions. Source stayed frozen throughout every gated compile/test batch.

## Final implementation / interfaces

- `operation_roots` preserves tree/input order while removing duplicates and selected descendants. Paths containing `..` stay until host canonicalization can prove ancestry, avoiding silent sibling omission. Drag, keyboard/menu copy, delete, clipboard/native transfer and host normalized paths share it. Folder+child operates on folder once.
- `FileTreeDragPayload` freezes original root, IO generation and `Arc<[PathBuf]>` at primary name-row drag start. Selection changes cannot alter it. A plain row scope avoids the built-in egui source reassigning its payload each drag frame. Existing single unselected drag and blank-space marquee behavior remain.
- `Move.sources` is a bounded list; `CopyInto.root: Option<_>` distinguishes rooted internal drag from authorized external copy. Default drag moves; Option at drop copies. Same-folder move is no-op; same-folder copy conflicts without replacement. Mixed no-op+move groups process remaining members.
- Shared bounds: **4,096 operation roots /32KiB individual raw encoded path /1MiB aggregate raw path bytes**. Native clipboard uses these constants. Whole overflow rejects with the existing visible error and retains selection, never primary-file fallback. Existing host recursive bounds stay50,000items/4GiB/depth128 for whole preflight; cross-device copy shares the operation budget.
- Shared `release_typed_dnd_payload` checks type before take. Tree header/rows, Workspace pane/terminal and attachment header preserve unrelated payloads. Groups emit all frozen paths; legacy PathBuf/external OS drops remain. Terminal file drops emit document intents without PTY writes.
- `PasteFromClipboard.move_files` implements Cmd+Option+V. This destructive gesture requires existing tree keyboard focus plus pointer ownership/text-edit/popup guards. Only initial non-repeat keydown starts movement; tree-owned repeat/release are consumed without another operation. Terminal-owned keys remain available. Ordinary paste still copies with existing semantics.
- Outside-tree move authority exists only on explicit clipboard paste. Internal Move still validates sources and destination under original tree root. Private fake clipboard proof copies and moves all outside-tree sources while the same rooted internal Move refuses them.
- `app_host_transfer_files` canonicalizes source parents without following final symlinks, validates/deduplicates again, and preflights all trees, destination names/conflicts, self/ancestor and copy canonical cycles before mutation. Execution reuses existing recursive copy, no-replace rename, cross-device fallback and removal engines. Native IO stays in App host workers.
- Group Trash preflights all members before first native callback and uses the canonical parent validated. Each capacity-1 continuation rechecks its own source against original root, denying redirected parents. Exact operation/generation completion and root-change cancellation remain. Native Trash failure preserves existing exact-target permanent-delete confirmation; no silent permanent fallback.
- The private native dispatcher accepts providers; tests inject private clipboard/Trash callbacks, and the test-only fixture bridge runs that same host engine. Tests invoke no OS clipboard or OS Trash.

## Actual before/after counts

| Scenario | Before | Executed final result |
| --- | --- | --- |
| Shift marquee + name drag |3selected→1source |3frozen→3moved; originals gone, all bytes matched |
| Option drop after marquee |Single-path payload |3frozen→3copied; originals preserved, all bytes matched |
|64selected files |Refused at16 |64copied+64moved+64private Trash effects; all64bytes/counts checked |
| Folder+child |Copied child twice |1root, child only inside copied folder |
| Last known conflict/cycle/missing deletion member |Earlier effects possible |0prior effects/native calls |
| Bounds |Tree16/256KiB; clipboard256/512KiB |4,096/1MiB exact accepted; +1 refused whole,4,097selection retained |

## Final gates / commands

`/tmp/deppy-pr11-final-source-gate-20261004.log` completed **exit0** on frozen final production/test source after the last parent-component correction, including fresh20-name inventory. Earlier19-case affected/inventory logs also completed exit0 but are historical. Every Cargo command used `/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py`, cwd the isolated worktree, locking compile plus execution. Arrays omit leading `cargo` because the wrapper supplies it.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[
 ["fmt","--all"],
 ["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr11_","--","--list"],
 ["test","--offline","--locked","-p","deppy-sijo","pr11_","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","ui::file_tree::tests","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","ui::clipboard_image::tests","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","ui::workspace::tests","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","app_host_","--","--nocapture"],
 ["run","--offline","--locked","-p","xtask","--","check-boundary"],
 ["clippy","--offline","--locked","-p","deppy-sijo","--all-targets","--","-D","warnings"],
 ["fmt","--all","--","--check"]
]' > /tmp/deppy-pr11-final-source-gate-20261004.log 2>&1
git diff --check
```

| Gate | Actual result |
| --- | --- |
| `pr11_` |20passed/0failed/0ignored;20fresh named tests |
| `ui::file_tree::tests` |181passed/0failed/2existing manual popup PNG tests ignored |
| `ui::clipboard_image::tests` |12passed/0failed/1existing real clipboard test ignored |
| `ui::workspace::tests` |330passed/0failed/1existing ignored |
| `app_host_` |7passed/0failed |
| `xtask check-boundary` |OK, zero allowlist capability |
| App all-target Clippy `-D warnings` |exit0/no warnings |
| fmt check/diff check |exit0 |

20 named cases cover actual Shift/Option gestures and private FS effects;64all-member operations; keyboard/menu roots; tree/terminal focus; actual held repeat/release; exactbounds/whole refusal; stale root/generation/old completion; incompatible consumers/actual terminal group and legacy drop; folder+child; last conflict/canonical cycle/unsafe member; same-folder/mixed no-op; fake clipboard authority; missinglast Trash; canonical Trash and changed-parent continuation; non-normal clipboard paths copying and moving both real siblings.

Final named inventory is the second command in the actual final-source batch above:20tests/0benchmarks. Final strict Clippy/fmt exited0. `git diff --check` also exited0. No source changes followed this batch before the initial PR11 commit; the corrective increment below has its own executed gates.

## Limits / handoff

All-before-effects refusal covers **known** invalid members, unsafe paths, conflicts and bounded preflight. Multiple filesystem operations are not a transaction: later OS races, native errors, cross-device failure or cancellation can leave earlier members completed. No rollback, blind retry, undo or all-or-none execution claim. Existing no-replace platform fallback semantics are retained.

Platform APIs materialize clipboard data before Deppy inspects size; bounds limit returned/retained requests, not that initial native allocation. Invalid native reads retain existing Option-return behavior. No actual Finder/clipboard/Trash or native-app visual smoke test, RSS/FPS/latency measurement ran. Terminal-group proof delivers every path to document intents; it does not claim unlimited document-tab admission.

Remaining root work: independent immutable code review, coherent combined gates, integration, version0.6.0 packaging/artifact checks without automatic launch. Child freezes scoped clean Korean commit after final named inventory and diff check; no further source edits planned. Root maintains global handoff/release/journal.

## PR11r — native gesture ownership and destination-volume conflicts

### Objective / actual source evidence

Corrective increment based on initial PR11 `9d3ae0d7232abc02d416f32686b473cf8304e717`, same isolated worktree. Independent immutable CLI review of the integrated source recorded two confirmed medium findings in `/tmp/deppy-final-followups-cli-result-20261004.txt`: real Cmd+Option+V keydown never reached the synthetic pressed-key start, and raw `OsStr` planned-name comparison missed destination-volume case/normalization equivalence. This section supersedes the initial native move-gesture completeness claim; earlier pressed-key fixtures did not reproduce the pinned backend.

Read actual primary source before correction:

- Pinned `egui-winit-0.36.1/src/lib.rs:1027` intercepts Command+V keydown, emits only `Event::Paste` for nonempty clipboard text, then returns even if text is absent. Its `is_paste_command` at line1430 does not exclude Option. Key release still emits `Event::Key { pressed:false }`. Local source: `/Users/jr/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-winit-0.36.1/src/lib.rs`.
- Darwin SDK `sys/unistd.h:154` exposes `_PC_CASE_SENSITIVE`; `sys/attr.h:180` documents volume case equivalence. Local primary headers are under `/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/usr/include/`. Unicode equivalence is not inferred from case metadata: the tests and fallback use the actual destination filesystem.

No native app, AppKit event generation, user clipboard, OS Trash or user files were used. Offscreen egui fixtures replay the backend's actual output shape and call the same native classifier/queue as production through a test bridge. Actual native volume fixtures create only UUID-owned temporary files/directories.

### Meaningful RED and failed approaches

| Log | Actual result / cause |
| --- | --- |
| `/tmp/deppy-pr11r-red-20261004.log` |exit101,0passed/2failed: native file-only gesture queued no move; case-equivalent second source conflicted after the first source had already copied. Both behavior assertions. |
| `/tmp/deppy-pr11r-initial-green-20261004.log` |exit0: initial2 corrections passed, prior20 PR11 cases passed. |
| `/tmp/deppy-pr11r-focused-green-20261004.log` |exit101,5passed/1failed: `Popup::open_id` alone did not render a popup, so `Context::any_popup_open` was false. Corrected fixture to actual `Popup::show`; ownership assertions stayed. |
| `/tmp/deppy-pr11r-final-focused-green-20261004.log` |exit0:7fresh named tests and7passed before final ownership review. |
| `/tmp/deppy-pr11r-owner-repeat-red-20261004.log` |exit101,0passed/1failed: a held native gesture begun under terminal focus emitted identity-free backend `Paste` after tree focus and started movement. Removed `Paste` as destructive start authority; native identity or typed nonrepeat pressed key is required. |
| `/tmp/deppy-pr11r-final-gate-20261004.log` |Behavior gates passed, then exit101 at strict Clippy `nonminimal_bool` in event retention. Equivalent DeMorgan simplification applied; no lint allowance. |
| `/tmp/deppy-pr11r-final-strict-green-20261004.log` |exit0 on final production/test source: fresh7-name inventory,7 correction tests,185 tree tests, App tail placement, boundary, strict Clippy and fmt. |

### Final production interfaces

- `NativeMovePasteGesture` carries a unique sequence and observation time in the existing bounded64-record native queue;500ms freshness remains. Native Command+Option+V observes `isARepeat`, records only a new nonrepeat gesture, and returns the original `NSEvent` unchanged. Existing ordinary copy/paste classification and printable/submit metadata remain separate.
- `take_clipboard_move_paste` consumes move-only records before FileTree ownership guards. Fresh native identity or a typed nonrepeat pressed key can authorize a destructive start; backend text `Paste` and keyup cannot. Held text repeat is consumed without another file operation, including a repeat after terminal/text/popup/hidden ownership changes. Distinct native gesture IDs work immediately, without the ordinary paste debounce. Original focus, pointer, popup, root/generation, bounds and capacity-one IO admission remain.
- Unconditional `App::ui` tail calls `discard_unclaimed_move_paste` after active and warm Workspace render effects and before frame statistics end. Home/Fleet/collapsed-sidebar/no-terminal frames therefore discard move-only metadata in the same frame. Actual caller inspection found no method-level early return bypass. The source placement guard runs, and actual hidden egui frames run the same discard function, preserve ordinary native paste metadata, deny later held-repeat rebinding and allow a fresh tree-owned gesture. This is not a full native-App UI smoke test.
- `app_host_preflight_destination_names` runs in the existing App worker before the source/content transfer loop. It uses the shared4,096names/32KiB individual/1MiB aggregate bounds and checks cancellation. Printable ASCII on verified APFS/HFS uses an owned directory descriptor, `fstatfs` and `fpathconf(_PC_CASE_SENSITIVE)` to compare raw/case-insensitive ASCII keys with zero temporary probes. Unknown volumes or Unicode names use create-new empty name probes in a UUID-owned hidden directory under the actual destination, following that volume's exact naming rules rather than an invented Unicode fold.
- The private probe directory is0700 on Unix, cleanup is attempted on every return/unwind, and explicit cleanup must succeed before transfer begins. Collision/cancellation fixtures leave no temporary entry. A cleanup failure refuses transfer and Drop retries cleanup; OS cleanup failure/process crash cannot guarantee no residue. Existing no-replace/cross-device transfer engines and internal tree-root versus explicit clipboard-origin authority remain.

**Known volume-equivalent names are refused before source/content transfer. Empty-file probes and their directory are filesystem metadata effects.** Whole-group execution remains nontransactional under later OS races, IO errors or cancellation; there is no rollback/all-or-none execution promise.

### Final named proof / executed gates

Seven `pr11r_` cases prove native gesture identity/freshness/repeat isolation; file-only backend output with no pressed-key event; text+file `Paste` repeat/release; terminal/TextEdit/rendered-popup ownership with held-repeat-after-focus-change; Home/Fleet/collapsed-sidebar MOVE-only expiry with ordinary metadata preserved; actual native case and NFC/NFD equivalence for copy and move;64-name metadata shortcut/probe counts and cancellation after the first actual probe. Both case/normalization tests first prove equivalence on the fixture volume. No ignored new case.

The expanded affected batch ran the following actual arrays, all through the shared gate, and stopped only at the Clippy Boolean finding described above:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[
 ["fmt","--all"],
 ["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr11r_","--","--list"],
 ["test","--offline","--locked","-p","deppy-sijo","pr11r_","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","pr11_","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","native_key_monitor::tests","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","ui::file_tree::tests","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","ui::workspace::tests","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","app_host_","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","app_flushes_workspace_render_effects_after_the_last_widget","--","--nocapture"],
 ["run","--offline","--locked","-p","xtask","--","check-boundary"],
 ["clippy","--offline","--locked","-p","deppy-sijo","--all-targets","--","-D","warnings"],
 ["fmt","--all","--","--check"]
]' > /tmp/deppy-pr11r-final-gate-20261004.log 2>&1
```

After the equivalent Boolean cleanup, final source was frozen for this successful batch:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[
 ["fmt","--all"],
 ["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr11r_","--","--list"],
 ["test","--offline","--locked","-p","deppy-sijo","pr11r_","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","ui::file_tree::tests","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","app_flushes_workspace_render_effects_after_the_last_widget","--","--nocapture"],
 ["run","--offline","--locked","-p","xtask","--","check-boundary"],
 ["clippy","--offline","--locked","-p","deppy-sijo","--all-targets","--","-D","warnings"],
 ["fmt","--all","--","--check"]
]' > /tmp/deppy-pr11r-final-strict-green-20261004.log 2>&1
git diff --check
```

| Gate | Actual result |
| --- | --- |
| Final `pr11r_` |7fresh names /7passed/0failed/0ignored |
| Final `ui::file_tree::tests` |185passed/0failed/2existing manual popup PNG cases ignored |
| Final App tail placement |1passed/0failed |
| Final boundary / App all-target Clippy `-D warnings` / fmt |exit0, no warning allowance |
| Expanded prior `pr11_` |20passed/0failed/0ignored |
| Expanded `native_key_monitor::tests` |8passed/0failed |
| Expanded `ui::workspace::tests` |330passed/0failed/1existing ignored |
| Expanded `app_host_` |7passed/0failed |
| `git diff --check` after final gate |exit0 |

Final64-name native preflight sample: printable ASCII **0 probes /2,607µs**;64 distinct Korean names **64 probes /7,760µs**, then zero leftover entries. Cancellation triggered after the first actual Unicode probe also cleaned the directory and refused transfer. These are one local private sample of added preflight work, not a native UI/RSS/FPS benchmark or guaranteed latency. Initial/expanded samples are historical and retained in raw logs.

### Scoped handoff

Changed only `crates/app/src/app.rs`, `crates/app/src/native_key_monitor.rs`, `crates/app/src/ui/file_tree.rs` and this report after9d3. No version/lock, global handoff, native app, user clipboard/Trash/data, PTY, push or agent delegation. No product/test source change after final strict batch; this documentation update records its actual result. Root owns immutable independent review, sequential integration with the PR10 App tail, combined gates and version0.6.0 release/package checks. Required next root commands: `git show --stat <frozen-child-sha>`; `python3 /private/tmp/deppy-audit-nine-pr-20261004/integrate.py 11r` (root preserves its real HEAD/index; the single App-tail overlap was manually merged preserving both tails); run the shared gate's coherent combined suite on the merged source. Root retains the unambiguous frame-tail ordering if PR10 adds a terminal input adapter there.

## Root final integration result — 2026-10-05

Finalsource d1818e3355e9604998e6581285bad8e7edd88ffb /productcommit c532ce0ad2c5edcc9e3dcbc779a61b37d5ea53a2. All confirmed follow-up/corrective CLI findings addressed; final tiny review no confirmed findings. Coherent fullsuite4,826passed/0failed/47existingignored, workspace strict/boundary/deps/fmt/diff exit0. Local0.6.0releasebuild/package/version/hash verified; no app stop/launch/restart. See [final results](2026-10-04-final-improvements-report.md) for final source and measurement limits.
