# PR5 — Session draft recovery, checkpointing and memory limits

## Scope and source

- Worktree: `/private/tmp/deppy-audit-pr5-20261004`.
- Branch: `fix/audit-pr5-session-drafts-20261004`.
- Baseline: `1aa5e622ea5c3b77af51d2683689c302e975c3b9` (integrated PR1/3/4/6/7).
- This report accompanies the scoped implementation commit. Root owns final integration, independent combined review, version increase and release build.
- No native Deppy launch, stop, restart, user PTY, system clipboard, live user files, version/lock change, push or delegation occurred. Tests use synthetic egui inputs and isolated temporary files.

## Changes

- Composer identity is a stable persisted session UUID, with stable pane UUID fallback and a separate workspace scratch draft. Workspace identity is retained separately for root/path and host continuation routing. Worker-local `SessionId(u64)` and runtime generation are not persisted draft identities.
- App matches the primary or attached target's actual runtime/pane, passes `draft_key` and `runtime_generation`, and keeps exact input target identity unchanged. Pending Composer origins carry `draft_key`; their actual workspace/runtime/session routing stays unchanged.
- App loads `composer_drafts.json` once. Missing files start empty; valid empty files stay empty. Corrupt, unreadable, over-budget, symlink/FIFO inputs retain their original bytes and enter visible recovery/read-only mode. Read-only Composer releases focus; unrelated terminal typing remains available.
- Live text is bounded at every input boundary: Unicode typing, IME, selection replacement/paste, history recall, palette insertion, model/tool snippets, file drops and asynchronous attachment completion. Refused insertions preserve the original text, cursor/selection and undo state. Existing dirty drafts are not evicted to make space.
- Only changed draft bodies become new `Arc<str>` checkpoint bodies; unchanged bodies are shared by cached/checkpoint snapshots. App checkpoints at most once per 500ms while edits continue and schedules a one-shot wake. File serialization and checked fingerprint comparisons are streamed on one lazy App-owned serial save worker. There is one active plus one coalesced latest request; no older revision can replace newer commit/status. Graceful shutdown and App/worker drop drain the latest accepted checkpoint.
- File creation is atomic no-clobber when missing; checked replacement preserves original on conflict/write/serialization error and cleans its private temporary file. File data is synced before rename. Directory rename durability across a power failure is not claimed.
- Composer submission freezes its draft revision. App bypasses the ordinary 500ms debounce to enqueue the Pending checkpoint immediately, then holds one immutable original target/prompt in a separate slot until the worker reports the required or newer successful save. Exact submission ID, payload, marker and runtime generation are revalidated before the existing tracked input path; PR2 actual AI execution checks remain at dispatch. Save/validation/conflict failures and retired/replaced targets reject known-unwritten input, retain the draft and never auto-retry. Worker completion wakes logic; workspace/modal controls and direct terminal keyboard input do not wait for this slot. The UI distinguishes “Saving before input…” from input acceptance.
- Pending/Unknown delivery uncertainty is persisted as a compact `delivery_uncertain` flag, including an empty draft when the user cleared its text. Restore blocks submission until explicit acknowledgment without fabricating an ACK, operation ID, history entry or an extra retained prompt payload. Exact Accepted/Rejected settlement and explicit acknowledgment invalidate the cached marker. Generation replacement retains uncertainty and rejects stale ACKs; no automatic resend occurs.
- Cleared normal drafts release their String allocations and map entries; empty uncertainty records retain only the bounded marker/owner. Large-shrunk dirty buffers compact when capacity exceeds twice their length or 64KiB, without copying stable-frame text.
- Runtime retirement/archival/workspace hiding/closing retain drafts. An admitted explicit permanent pane close gets a unique existing durable FIFO barrier; only that runtime's correlated barrier and authoritative missing pane authorize deletion. Active workspace cleanup suppresses permanent deletion only for the exact old pane IDs in `ClosingPanes`; an explicitly closed new pane remains eligible while old cleanup acknowledgments are pending. Arbitrary/incomplete snapshots never prune drafts. Successful workspace database deletion retires all owned drafts and delivery retention, including cleared drafts.
- One fixed active TextEdit ID uses the shared eight-undo policy. Session switches reset that history while keeping inactive text/caret anchors, preventing cross-session undo or per-session egui history accumulation.

## Explicit budgets

| Payload | Bound | Notes |
|---|---:|---|
| Live draft text | 1MiB each / 32MiB total | 256 records; empty uncertainty records count toward items |
| Live String capacity after compaction | ≤64MiB + 16MiB slack | ≤sum(max(2×length,64KiB)); empty buffers retain no capacity |
| Draft key / workspace ID | 8KiB / 4KiB | Aggregate persisted key+owner text 256KiB |
| Encoded checkpoint file | 64MiB | Streaming reader/writer; excessive JSON escaping reports unsaved state |
| Pending attachment paths | 32KiB per root/path, 16 paths / 256KiB payload | Two latest host requests; captured actual workspace/root |
| Composer delivery payload | 8MiB / 256 entries | Separate from live drafts; receipt key+owner total 256KiB; confirmed deletion releases it |
| Send history | 1MiB / 100 entries | Existing bounded Arc history, separate from drafts |
| Active undo | Eight accepted snapshots | At most eight 1MiB snapshots plus egui flux/current state |
| Save requests | One active + one latest | Arc-shared unchanged bodies; bounded metadata copies |
| Deferred durable-input slot | One prompt ≤1MiB | Shares receipt Arc; frozen target/key; independent controller actions |

32MiB is the **logical live text budget**, not App RSS. Cached Arc bodies and old worker checkpoints may retain additional bounded versions; each full logical checkpoint is at most 32MiB. During snapshot replacement there can transiently be cached/new, previous pending and active versions (a conservative 96MiB Arc body payload accounting bound before sharing), alongside live String capacity of at most 80MiB after compaction. Token stripping can transiently retain up to two extra 1MiB String bodies during checkpoint construction. Undo/flux and overflow-only rollback snapshots, delivery payload, history, parser scratch, egui galleys and allocator overhead are additional. Unchanged-body sharing is verified with actual `Arc::ptr_eq`, not inferred from a prototype. Stable UI frames do not clone whole draft bodies or undo history.

Startup deserialization streams encoded input through a 64KiB buffer and rejects record count/content/metadata limits as records are decoded. serde's temporary string scratch for malformed oversized fields remains bounded by the 64MiB encoded file limit; this is not a claim of a 64KiB total parser allocation. Raw integration-owned paste/IME event strings can already be allocated before the field receives them; this change bounds retained Composer text and guarded mutation, not upstream event allocation.

## Observed RED/GREEN and failed approaches

1. `/tmp/deppy-audit-pr5-red-20261004.log`: baseline plus tests — real same-workspace A/B session typing and actual temporary-file restart recovery failed (2 failures). The initial click-only IME fixture passed because it did not retain focus; that pass is excluded as input proof.
2. `/tmp/deppy-audit-pr5-input-red2-20261004.log`: temporarily restoring the actual legacy unbounded TextEdit boundary in the isolated current UI — focused IME Preedit/Commit and ordinary Korean typing grew a 1,048,575-byte draft to 1,048,578 bytes (2 failures). Restored bounded production editor keeps 1,048,575 bytes. Oversized selection paste preserves exact original text and selection.
3. `/tmp/deppy-audit-pr5-green2-20261004.log`: first integration passed three named tests but failed two old attachment/caret tests. One fixture bypassed the new active-identity switch seam; adjusted it to exercise the actual switch. One production active-caret branch accidentally used the inactive anchor map; fixed to read the active fixed editor state. `/tmp/deppy-audit-pr5-green4-20261004.log`: all existing Composer 67 tests passed after these corrections.
4. Strict Clippy initially found two style issues (`nonminimal_bool`, `collapsible_if`); corrected. A new IME fixture initially used the outdated `cursor` field/Enabled event; replaced with current `active_range_chars` and actual focused preedit/commit.
5. `/tmp/deppy-audit-pr5-marker-red-20261004.log`: temporarily restoring legacy text-only recovery dropped the saved uncertainty marker; actual save→load→Composer restore allowed a pending prompt to be sent again (1 failure). Restored production marker recovery blocks it until explicit acknowledgment, including empty Unknown drafts, with no fabricated receipts.

6. `/tmp/deppy-audit-pr5-marker-final-proof-20261004.log`: 24 named tests passed; an old PR1 fixture directly changed a delivery enum, bypassing the actual explicit acknowledgment that now also clears the side marker. Updated that fixture to the real acknowledgment seam; final Composer72 passes.
7. `/tmp/deppy-audit-pr5-capacity-red-20261004.log`: 300 cleared 32KiB buffers retained 9,830,400 bytes of actual `String::capacity` despite zero live text. Final production checkpoint releases all 300 entries and retained capacity to zero; empty Unknown records have zero capacity, and a 1MiB→one-character dirty buffer compacts below 64KiB. This is counted retained component capacity, not App RSS.
8. Independent review found active workspace close generated permanent markers through the normal ClosePane drain while warm close did not. Production eligibility now excludes only captured cleanup pane IDs. Regression uses the real `WorkspaceUi::close_pane_now` producer, old cleanup/fresh permanent panes, exact barrier and missing mux. The first source-binding fixture used the reverse spelling of the actual condition and failed (`Option::unwrap`); corrected the source binding rather than claiming that attempt passed.
9. `/tmp/deppy-audit-pr5-dispatch-red-20261004.log`: legacy immediate-input policy returned Dispatch for Pending/old Saved checkpoints (observed Dispatch vs Wait failure). Final policy waits for successful required/newer save and rejects save failure/conflict/missing marker. Actual temporary file + worker test begins with old false marker on disk, saves/coalesces the current true marker, restores it as blocked, retains the original session/prompt despite other-session edits, and permits exactly one dispatch. Known-unqueued rejection after runtime retirement keeps text and never sends when saving is retried.

## Verification

All Cargo invocations use the root gate, which locks compile **and** execution and cleans workspace package artifacts on a worktree source switch. No ungated/cross-worktree cached test counts are claimed.

Final immutable-source candidate proof: `/tmp/deppy-audit-pr5-durable-final-proof-20261004.log` — **29 named PR5 tests executed/listed; Composer72; i18n8; strict App all-target Clippy (`-D warnings`); fmt check**, all passed with gate process exit0. `git diff --check` also exit0. Earlier pre-marker proof22/Composer69 and intermediate failed gate attempts are retained as history, not final-source claims. Named PR5 and Composer groups overlap; do not sum them as independent tests.

```sh
cd /private/tmp/deppy-audit-pr5-20261004
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[
 ["fmt","--all"],
 ["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr5_","--","--test-threads=1"],
 ["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr5_","--","--list"],
 ["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","ui::composer::tests","--","--test-threads=1"],
 ["test","--offline","--locked","-q","-p","i18n"],
 ["clippy","--offline","--locked","-q","-p","deppy-sijo","--all-targets","--","-D","warnings"],
 ["fmt","--all","--","--check"]
]'
git diff --check
```

Tests cover real same-workspace different-session editors; Unicode IME/paste/selection; stable persisted identity vs replaced u64/pane; original root after late completion; deletion/generation rejects late result/ACK; repeated Unknown retained-byte budget; cleared draft/workspace delivery purge; actual bounded undo and A→B undo isolation; original/corrupt/empty/readfailed/symlink/FIFO/escaped-file preservation; exact worker order/coalescing/stale request/status/drop drain; visible retry intent; durable Pending/Unknown restore and old-format default; cleared capacity/shrink; actual active workspace cleanup versus fresh permanent close; durable-before-input host gating with actual file/worker/coalesced revision and no duplicate dispatch.

## Integration interfaces

- `ComposerContext { workspace_id, draft_key, runtime_generation, ... }` — root/path use `workspace_id`; draft ownership and UI persistence use `draft_key`.
- `ComposerUi::{restore_drafts(DraftSnapshot), draft_revision(), checkpoint()->Arc<DraftSnapshot>, active_draft_key(), set_read_only(bool), retire_generation(ctx,generation), delete_draft(ctx,key), delete_workspace_drafts(ctx,workspace)}`.
- `DraftRecord { key, workspace_id, text:Arc<str>, delivery_uncertain:bool }`; the boolean has `serde(default)` for old fixtures/formats.
- `DraftSnapshot::{validate, load_startup, save_checked}` and `DraftSaveWorker::{new, request, status, path, shutdown}`. Revision admission is strictly increasing; App save revision and content/checkpoint revision are separate for explicit retry.
- `ComposerSubmission::required_draft_revision()` freezes the Pending mutation before any later text changes. `WorkspaceControllerAction::ComposerPrompt` carries this revision; `stage_composer_prompt_action` accepts it as its seventh argument.
- `ComposerUi::{pending_submission_matches(key,id,prompt,generation), mark_submission_dispatched(...), reject_unqueued_submission(...)}` preserves exact original-generation known-no-effect settlement at the deferred gate.
- App `PromptDeliveryOrigin::Composer` / `WorkspaceControllerAction::ComposerPrompt` field renamed `draft_workspace_id` → `draft_key`; actual pending workspace/runtime/session fields remain actual. Preserve PR2 `PromptInputContext::Ai` and exact execution guards during root's three-way App integration.
- Shared PR4 `ui::text_input` helper was reused and **not modified**; root's Fleet helper work remains separate.

## Completion and limitations

- [x] Session identity, draft recovery, bounds, async save and visible errors implemented.
- [x] Exact old-generation attachment/ACK protection and pending/Unknown checkpoint marker implemented.
- [x] Pending marker saved before tracked Composer input; save failure/no effect rejection and one-time frozen target dispatch verified.
- [x] Cleared/shrunken String capacity released and active workspace cleanup preserves drafts.
- [x] Confirmed deletion only; hidden/restorable drafts and original files retained.
- [x] Scoped source review against baseline; failure fixes and actual proof recorded.
- [ ] Native app/provider TUI, App RSS/GPU/FPS and Windows execution were not run. Root owns combined review/tests and release build without launch.

Successful ordinary 500ms checkpoints and normal shutdown/drop preserve draft files; abrupt process death before an ordinary checkpoint may lose recent unsent text. Tracked Composer input specifically waits for the Pending marker file save, so a process crash after dispatch restores a conservative Unknown warning. A crash after marker save but before input may also restore that warning even though nothing was sent. Directory-sync/power-loss durability is not claimed. This remains a draft checkpoint and write-ahead warning feature, without a claim of exactly-once agent execution. Scratch drafts remain independent from subsequently created sessions; no hidden migration or automatic transmission occurs.
