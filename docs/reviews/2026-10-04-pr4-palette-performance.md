# PR4 — Prompt palette caching, bounded input and streaming verification

## Scope and source

- Isolated branch: `fix/audit-pr4-palette-cache-wave2-coherent-20261004`.
- Baseline: `ed2c9163b21516b704234d5c20360dfa80d2d2bc` (integrated PR1/PR3/PR6).
- Product version remains inherited 0.5.5; this is a scoped implementation/test commit. The orchestrator owns release/version/package verification.
- Modified production files: `crates/app/src/{app.rs,prompt_library.rs,ui/prompt_palette.rs,ui/text_input.rs,ui/mod.rs}`, all 5 i18n message catalogs. Existing prompt Window/Escape/read-only/rejected-form behavior retained. No new shell migration or modal introduced.
- No native Deppy launch/stop/restart, user PTY execution, real user library access, push, lock/version or shared handoff edits.

## Implemented checklist

- [x] App skips closed palette rendering and borrows `composer.current_text` when open; it no longer clones the draft every frame.
- [x] `PromptSearchCache` owns query/contentrevision and at most 1024 indices, no borrowed prompt references or permanent lowercase body copies. Exact unchanged query+revision reuses the result. Changed query/revision retains existing Unicode lowercasing semantics.
- [x] Fixed two-line nonwrapping results use actual `ScrollArea::show_rows`; title/tag formatting and text layout only run for visible rows. UTF-8-safe tooltip borrows the first 16 KiB and shows truncation notice.
- [x] Selected template caches bounded parameter names and expansion, invalidates on selection/contentrevision/actual input changes, clears stale expansion on failure, and disables Insert on error. Fixed 128 parameter field IDs do not accumulate egui histories for arbitrary names across templates.
- [x] Expanded text is capped before allocation at1MiB, values8KiB each/64KiB combined, names128/256B each/16KiB combined. Preview layout uses only first 16 KiB, while Insert returns the complete bounded expansion.
- [x] Live query256B/title4096B/body1MiB/taginput8224B/parameter fields use a byte-aware TextBuffer. Whole over-limit insertions/replacements are refused without UTF-8 truncation. An overflow-only snapshot restores original selection/preedit/undo state after egui deletes before inserting; stable frames/ordinary typing do not clone full bodies. Enter/Tab/Undo/Redo paths are included. Existing rejected forms over a new limit remain intact and allow deletion/correction.
- [x] Shared `ui::text_input` applies an 8-point undo limit per fixed field, initialized once without stable-frame history copies. New/Edit/composer-prefill resets the 3 fixed editor histories once; rejection of the exact submitted form preserves valid undo. This prevents undoing prompt B into canceled prompt A content.
- [x] Composer-prefill button is disabled before cloning a body over1MiB. Visible translated limit/truncated-preview notices exist in all 5 locales.
- [x] Checked-save file fingerprints use a bounded64KiB stack scratch reader with limit+1/type/NOFOLLOW/NONBLOCK protections, avoiding two up-to16MiB Vec reads per save. Startup still uses the original bounded full JSON read. Atomicrename/missing seed no-clobber/conflict guards retained.
- [x] Test-only `PromptLibrary::{load,try_upsert,save,search}` aliases are `cfg(test)`, resolving production dead-code from PR3 without unnecessary work or warning suppression.

## Observed RED/GREEN

`/tmp/deppy-audit-pr4-red-20261004.log` records actual source assertions before fixes:

1. Same query/contentrevision rescan counter44 instead of4 after10 stable frames.
2. Actual palette rendered all1000 result rows.
3. Byte adapter inserted2 Korean characters despite insufficient byte budget, expected0.

Additional actual REDs:

- `/tmp/deppy-audit-pr4-undo-red-20261004.log`: real TextEdit allowed 20 full-snapshot undo steps after 20 stabilized edits, exceeding the new 8-point policy.
- `/tmp/deppy-audit-pr4-form-red-20261004.log`: editing focused A to `A_BODYK`, canceling, then editing B and pressing CmdZ replaced `B_BODY` with `A_BODYK`. The first never-focused-A fixture passed because egui only fed undo for focused fields; corrected to real typing before observing the failure.

The first RED invocation failed compilation because the egui TextBuffer adapter needed `type_id`; the corrected baseline adapter then produced all3 assertion failures. An initial GREEN compile attempt used obsolete tuple syntax for egui0.36.1 `ImeEvent::Preedit`; corrected to its actual struct variant. The first undo helper build also referenced a private egui alias; corrected to public generic `egui::util::undoer::Undoer`. None of these compile failures is reported as a behavioral RED.

Final frozen-source gate: `/tmp/deppy-audit-pr4-final-proof-20261004.log`.

- Named `pr4_`:14 passed,0 failed,14 listed. Actual result rows:11/1000.
- `prompt_palette`:12 passed; includes original Escape ownership/read-only preservation plus actual select/complete Unicode insert/edit/save/delete.
- `pr3_`:20 passed, preserving startup/worker/revision/conflict recovery behavior.
- i18n:8 passed.
- Strict App all-target Clippy: exit0, warnings denied.
- `git diff --check`: exit0.

The named tests include actual egui TextEdit overflow Paste and IME Commit with a selection, accepted Korean replacement, Enter overflow on existing rejected text, cached expansion invalidation/recovery, name/aggregate/expanded budgets, Unicode search equivalence, streaming fingerprint/chunk-boundary/actualcheckedsave and oversized/nonregular/conflicting files. Additional cases verify actual CmdZ still works after refused Paste/IME and within the 8-point limit, reset across distinct forms, and exact rejected-form undo retention. They use temporary fixtures, no native application.

All Cargo commands run through the orchestrator exclusive gate, which cleans workspace artifacts on worktree source switches and holds the lock through compilation/test execution. Initial/prior gates are superseded by the final frozen-source log.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr4_","--","--test-threads=1","--nocapture"],["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr4_","--","--list"],["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","prompt_palette","--","--test-threads=1"],["test","--offline","--locked","-q","-p","deppy-sijo","--bin","deppy-sijo","pr3_","--","--test-threads=1"],["test","--offline","--locked","-q","-p","i18n"],["clippy","--offline","--locked","-q","-p","deppy-sijo","--all-targets","--","-D","warnings"]]'
```

## Actual before/after measurements

Harness: `/private/tmp/deppy-pr4-bench-20261004/src/main.rs`; imports actual baseline `prompt_library.rs` copied with `git show ed2c916:...` and the production PR4 source by absolute path. Log: `/tmp/deppy-audit-pr4-bench-20261004.log`. Release build, System counting allocator,5 sample medians, fixture creation outside measured sections. Each template has 8000B of body;100/1000 prompts =0.8/8MB. Warm cache uses the actual new cache, not a normalized prototype. Save measurements use the actual worker's `save_checked`, including both version checks/fsync; serialization-only save would understate the old allocation.

| Operation | Before | After | Cumulative allocated bytes before→after |
| --- | --- | --- | --- |
| Closed1MiB draft handling |12.713µs clone |0ns borrow benchmark resolution |1,048,575→0 |
|100prompt miss search |1.620ms |1.596ms cold /2–3ns unchanged |801,501→801,501cold /0warm |
|1000prompt miss search |16.224ms |16.084ms cold /2–3ns unchanged |8,015,901→8,015,901cold /0warm |
|100prompt actualcheckedsave |4.992ms |5.007ms |4,260,154→65,911 |
|1000prompt actualcheckedsave |15.006ms |13.993ms |33,620,285→65,914 |
|1000result row work |1000rows |11rows |actual egui counter, no allocation claim |

Warm English/Korean/miss/empty queries each allocate0B and preserve expected result indices. Search cold misses still allocate transient lowercase strings and take similar time; retaining8MB of normalized body copies was deliberately avoided. Checked-save100prompt disk latency is unchanged/slightly higher within localfsync measurements; the confirmed improvement is bounded verifier heap allocation. These are module timings/cumulative allocator bytes, **not App RSS, native UI frame rates or GPU measurements**. Query cache retained indices are at most8192B on64-bit, selected expanded payload at most1MiB; string/map/egui glyph overhead remains distinct from logical byte limits. Accepted body undo points have at most 8×1MiB logical payload, plus egui's in-progress flux snapshot; overflow/Undo rollback can temporarily duplicate that bounded history. Parameter IDs are fixed at128 rather than growing across arbitrary template names. These are bounds, not measured RSS. Integration-owned incoming Event strings remain outside this guard's allocation budget: a huge already-received singleline Paste can be copied by egui newline normalization before TextBuffer refusal. Accepted field/output bytes are bounded; early clipboard/input-layer budgeting is separate follow-up work, not claimed solved here.

Reproduction:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["generate-lockfile","--offline","--manifest-path","/private/tmp/deppy-pr4-bench-20261004/Cargo.toml"],["run","--offline","--locked","--release","--manifest-path","/private/tmp/deppy-pr4-bench-20261004/Cargo.toml"]]'
```

## Integration contract / remaining orchestrator work

- `PromptPaletteUi::render(ctx: &egui::Context, library: &PromptLibrary, library_revision: u64, composer_draft: &str, catalog: &i18n::Catalog) -> Option<PromptPaletteAction>`; composer draft remains borrowed for PR5 session-key integration.
- Contentrevision changes only after accepted actual content mutations (PR3). Save retry/completion revisions do not invalidate palette caches.
- Pure APIs: `param_names_bounded(&str) -> Result<Vec<String>, PromptLibraryError>` and `render_bounded(&str, &BTreeMap<String,String>) -> Result<String, PromptLibraryError>`; `PromptSearchCache::update(&PromptLibrary,u64,&str) -> Result<&[usize],PromptLibraryError>`.
- Shared APIs in `ui::text_input`: `initialize_bounded_undo(&egui::Context, egui::Id)`, `forget_bounded_text_state(&egui::Context, egui::Id)`, `bounded_edit(&mut egui::Ui,&mut String,usize,egui::Id,&str,bool) -> (egui::Response,bool rejected)`, all `pub(crate)`; `BoundedTextBuffer` and its fields are also crate-visible for custom layout reuse. Initialize before first TextEdit/caret restoration; use a fixed active-editor ID and forget it when the content identity changes. Do not accumulate one history per persisted session/template name. `bounded_edit` currently retains palette singleline/fullwidth and multiline7rows/code-editor presentation; callers with specialized composer layout may reuse the adapter/undo setup and the same conditional rollback semantics.
- `set_read_only` and `restore_rejected_prompt` remain unchanged. Load failures still preserve original readonly data; no partial oversized-template insert.
- Root owns Fleet broadcast/batch caller glue after PR2+PR4 merge: migrate the remaining `prompt.params()` / unbounded render blocks to these Result APIs, show `prompt.input_limit`, disable submission on failure. One oversized-parameter legacy template must not make the whole valid library readonly. No unsafe partial execution. Old pure parser/render helpers remain compatible until those callers migrate; root can mark test-only after migration.
- Root final composed tests/review/release still required. No native UI/RSS claim.
