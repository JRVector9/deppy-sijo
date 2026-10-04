# PR20 — move layout classification and portable volume fixtures

## Objective / source

Started 2026-10-04; completed 2026-10-05 KST. Isolated `/private/tmp/deppy-audit-pr20-20261004`, branch `fix/audit-pr20-move-layout-volume-fixtures-20261004`, exact baseline `832cb06ed83d4cacf7054b80cf5799a94584270c`. Read applicable AGENTS and the actual `gpt-6.1-sol`/`xhigh` immutable review `/tmp/deppy-final-corrective-cli-result-20261004.txt`. TDD/workstep guidance applies. Root owns CLI review, global handoff, coherent full gates, version/release/artifact work. Child invoked no CLI or other agent.

Two confirmed findings were scoped here: the Option move branch reused ordinary paste's unconditional physical V fallback, accepting Dvorak logical K; new native-volume fixtures assumed case insensitivity and NFC/NFD equivalence. Changed only `native_key_monitor.rs`, App tests, a FileTree test and this report. Product App UI, filesystem engine, tree handler, root/version/lock/xtask files are untouched.

## Actual RED

All Cargo used `/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py`, cwd this isolated worktree. Arrays omit leading `cargo`; compile and execution remain under the shared lock, including cleanup after source-path changes. Source was frozen throughout each batch.

| Log | Executed result |
| --- | --- |
| `/tmp/deppy-pr20-layout-red-20261004.log` |exit101,0passed/2failed: actual production native classifier recorded move for physical0x09 plus logical `k`; real focused egui tree with backend-shaped logical KeyK queued destructive file request. |
| `/tmp/deppy-pr20-volume-fixture-red-20261004.log` |exit101,0passed/1failed: adding deterministic distinct names to the inherited fixture failed its read-alias assumption (`NotFound`). This reproduces the fixture assumption on the current private volume; no case-sensitive volume was mounted. |
| `/tmp/deppy-pr20-targeted-green-20261004.log` |exit0:2 PR20 tests and7 corrected PR11r tests passed. |
| `/tmp/deppy-pr20-affected-strict-green-20261004.log` |exit0: fresh2-name inventory, native9 tests, tree186 tests, strict App all-target Clippy and fmt. |

Exact RED commands:

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-p","deppy-sijo","pr20_","--","--nocapture"]]' > /tmp/deppy-pr20-layout-red-20261004.log 2>&1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-p","deppy-sijo","pr11r_native_volume_case_and_normalization_conflicts_precede_all_transfers","--","--nocapture"]]' > /tmp/deppy-pr20-volume-fixture-red-20261004.log 2>&1
```

No compilation/fixture-API failure or further failed approach occurred in this increment.

## Implementation / proof

- New move-only `is_clipboard_move_paste_key` accepts logical ASCII V in either case at any physical key; rejects other supplied ASCII strings; and permits physical ANSI V fallback only for non-ASCII, missing or empty characters. Command and the existing Option/Control/Function gates remain. Ordinary copy/paste predicates and behavior are unchanged.
- The test bridge feeds the same production classifier/queue used by the AppKit callback without generating a native event. Native unit proof rejects `k`, `K`, `c`, `1`, space and `vk` at physical V; accepts logical V on another physical key plus Korean/Option-generated/missing/empty physical V; blocks each held repeat; and retains legacy ordinary paste/copy classification.
- Actual offscreen egui registers tree keyboard focus and pointer ownership. Dvorak logical K plus physical V is replayed as the pinned backend's KeyK, without a synthetic KeyV/Paste start, and queues no file request after the fix. Logical V on a different physical key, Korean and missing characters then each produce a file-only native move request, with exact IO settlement between gestures. Backend text Paste on held repeat queues no further request. Product tree input logic is unchanged.
- App volume fixture now observes destination `create_new` behavior for each pair before testing transfer. An `AlreadyExists` observation requires Conflict, empty destination/probe cleanup and all original source bytes retained. A successful second create requires both members' destination bytes/counts, originals preserved for copy and both originals absent for move. Unexpected observation errors fail the test. Case, NFC/NFD, deterministic distinct ASCII and distinct Korean pairs all execute for copy and move; no skip/ignore.
- The64-name fixture derives ASCII probe count from actual recognized-volume metadata: known case-sensitive or case-insensitive APFS/HFS requires0 probes, unknown metadata requires64. Distinct Unicode names still use64 actual destination probes. Cancellation after the first actual probe still refuses transfer and verifies no leftover probe directory. No product filesystem-preflight change.

On the actual local volume, observed case and NFC/NFD pairs were equivalent; deterministic ASCII and Korean pairs were distinct. Both conflict and successful all-member copy/move branches ran. Metadata was `Some(false)`, ASCII64 produced0 probes and Unicode64 produced64. The targeted log records one preflight sample of1,044µs/6,861µs; it is not a promised latency or native-UI benchmark. A case-sensitive/unknown volume was not created or mounted; expectations now follow observed semantics when those configurations execute these same tests.

Empty fixture/probe files are private filesystem metadata effects. Observed-equivalent groups still require zero source/content transfer. Existing filesystem execution remains nontransactional under later OS races/errors; no rollback or all-or-none execution claim.

## Actual GREEN gates

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[
 ["fmt","--all"],
 ["test","--offline","--locked","-p","deppy-sijo","pr20_","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","pr11r_","--","--nocapture"]
]' > /tmp/deppy-pr20-targeted-green-20261004.log 2>&1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[
 ["test","--offline","--locked","-p","deppy-sijo","--bin","deppy-sijo","pr20_","--","--list"],
 ["test","--offline","--locked","-p","deppy-sijo","native_key_monitor::tests","--","--nocapture"],
 ["test","--offline","--locked","-p","deppy-sijo","ui::file_tree::tests","--","--nocapture"],
 ["clippy","--offline","--locked","-p","deppy-sijo","--all-targets","--","-D","warnings"],
 ["fmt","--all","--","--check"]
]' > /tmp/deppy-pr20-affected-strict-green-20261004.log 2>&1
git diff --check
```

| Gate | Actual result |
| --- | --- |
| Fresh `pr20_` inventory |2tests/0benchmarks |
| Targeted `pr20_` |2passed/0failed/0ignored |
| Corrected `pr11r_` |7passed/0failed/0ignored |
| `native_key_monitor::tests` |9passed/0failed/0ignored |
| `ui::file_tree::tests` |186passed/0failed/2existing manual PNG cases ignored |
| App all-target Clippy `-D warnings` |exit0, no lint allowance |
| fmt / diff check |exit0 |

## Scoped handoff

Product/test source was unchanged after the final strict batch; this report records executed results. No native app launch/restart/stop, real user clipboard/Trash/data/PTY, secrets, version/lock change, root documentation, push or delegation. Root's earlier0.6.0 artifact remains unreleased; this child does not claim release completion.

Next root commands in the approved integration worktree: `git show --stat fix/audit-pr20-move-layout-volume-fixtures-20261004`; `python3 /private/tmp/deppy-audit-nine-pr-20261004/integrate.py 20` (completed while preserving real HEAD/index). Root then runs the corrected coherent full gate and the scoped `gpt-6.1-sol`/`xhigh` immutable CLI review before updating its package. Child freezes the scoped Korean commit and waits.

## Root final integration result — 2026-10-05

Integrated clean6a796b5 into final freeze d1818e3/sourcec532ce0. Tiny immutable CLI confirmed both findings addressed with no confirmed new issue. Coherent full suite4,826passed/0failed/47existingignored; allstrictgates passed. Local0.6.0 build/package/version verification passed, app was not launched or restarted. See consolidated final improvements report.
