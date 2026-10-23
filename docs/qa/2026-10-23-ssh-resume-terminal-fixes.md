# SSH sleep recovery, terminal correctness and path links — 2026-10-23

Source-only implementation in `/Users/jr/Desktop/projects/deppy-sijo-integration-20261023`, branch `fix/product-review-integration-20261023`, base `a8eb51ae5d8cdaaff2b9246ad96aeb474d724ed1`. The nominal `deppy-sijo` tree was not edited. User requested parallel implementation; all delegated agents used GPT-6.1-Sol/xhigh with disjoint editing ownership. Root integrated/reviewed changes and serialized every Cargo command.

No app launch/restart, installed helper update, live user SSH, commit, push or release was performed. Source version remains 0.17.0 for investigation/test artifacts; the running 0.16.0 app and previously packaged 0.17.0 bundle remain unchanged. A future distributed release must increase the version and verify all artifact versions.

## Resulting behavior

- The stdio bridge duplicates stdout with CLOEXEC, uses actual nonblocking writes and cancellable 100ms polls, and restores original file flags. Stdin EOF cancels the output worker before joining it. The existing 45s no-progress deadline resets after partial progress. A live reader that stops draining stdout cannot trap shutdown indefinitely.
- SSH supervisor detached retention defaults to **604800 seconds (7 days)**; the existing `DEPPY_REMOTE_LEASE_SECONDS` override accepts 1–2592000 seconds (30 days). Invalid values fall back to the default. Both protocol negotiation and the client validate this same range; wire shape/version is unchanged.
- A previously authenticated workspace retries transient network/worker loss after 1, 2, 4, 8, 16, then at most 30 seconds. Automatic attempts require the existing exact-target resume proof and cannot create a fresh remote runtime if the proof is missing/expired. Host/account/key/root mismatches, authentication, helper or expiry errors require explicit recovery. Cancel, workspace closure and retired targets stop retry; retired targets also lose queued/visible authentication dialogs. No terminal input or commands are retained/replayed by retry logic.
- Selection freeze and Home/Fleet catch-up invalidate lost dirty-row history. Consecutive visible snapshots retain incremental rendering; idle frames retain cached rows. Tests draw actual egui text rather than only inspecting counters.
- Memory archive serialization retains projected foreground/background, bold, dim, italic, underline and strikeout, including styled trailing spaces and compressed history. Concealed source text and combining marks are omitted irreversibly; occupied columns are preserved as spaces. Every output append obeys the archive byte bound.
- Visible file paths support single-click activation through the existing deferred resolver. Paths spanning visible rows, Korean wide glyphs/graphemes, balanced filename punctuation and optional `:line[:column]` suffixes are recognized. Exact raw filenames win over wrapper/suffix fallback, so `README)` cannot silently become `README`. Files must actually resolve before opening.
- `.md`, `.markdown`, `.txt`, `.log` and extensionless documents open in the existing session-scoped app document tabs. Other supported local formats retain the existing OS-open lane; the OS allowlist is unchanged. Numeric suffixes locate the file but do not implement a document line jump. URLs/folder clicks, drag selection and first-click focus on another pane retain their behavior; alternate-screen apps retain pointer ownership.
- SSH path lookup uses only authenticated root-scoped ListDirectory RPC, bounded to 4096 entries/1MiB over candidate parents; it rejects symlinks, traversal and local fallback. The existing remote loader validates regular files with nofollow/nonblocking opens. Unsupported remote formats cannot reach a Mac OS opener. Remote `~/` expansion is not inferred from this Mac's HOME.
- Pending and already-queued file activation is cancelled on Home/Fleet, workspace/pane/session or terminal-input-owner changes. App dispatch rechecks active workspace/runtime, primary-surface focus and Terminal view. Late replies preserve their originating runtime identity even when a new WorkspaceUi reuses operation/generation numbers.

TCP cannot remain physically connected while the Mac is asleep. Recovery preserves/rejoins the remote supervisor/PTY while its lease and server remain alive. Server reboot/forced termination and expiry cannot be repaired by reconnecting to that original PTY.

## Verification

Final verification evidence is collected in `assets/ssh-resume-terminal-fixes-20261023/`; exact commands, outcomes and source SHA256 manifest accompany the logs. All commands use `--offline --locked`, `CARGO_BUILD_JOBS=2`, and `CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target`.

| Check | Executed result |
|---|---|
| remote-host full suite | 33 unit + 7 integration passed; 3 isolated child fixtures ignored at top level and invoked by parent tests |
| terminal full suite | 147 passed, 4 ignored; compressed-history conceal/style round-trip included |
| runtime full suite | 459 passed, 1 ignored |
| ssh-transport full suite | 24 passed, 2 ignored |
| App full suite, first coherent integration | 3768 unit + 39 integration passed; 46 unit + 3 integration ignored |
| App final scope corrections | 3773 unit + 39 integration passed, 0 failures; 46 unit + 3 integration ignored |
| Strict Clippy on 5 affected packages/all targets | Passed on final source with `-D warnings` (14.823s) |
| Feature disabled compile | Final `cargo check -p deppy-sijo --no-default-features` passed (3.730s) |
| Format / architecture boundary | Final `cargo fmt --all --check` (2.854s) and `cargo run -p xtask -- check-boundary` (4.884s) passed |

**4482 distinct normal tests passed across the five packages** (isolated child invocation results excluded from this sum). The final App suite includes 40 path tests and 9 retry-policy/integration tests. Focused coverage is contained within the full suites: real stdout pressure/closed reader/max-size RPC, slow reader progress, isolated supervisor lease environment boundaries, automatic retry/fatal/manual/cancel/close/stale identity and launcher suppression, renderer skipped generations/freeze/Home, archive bounded/compressed history, actual egui pointer click/drag/cold lookup and dispatch/runtime/root/attached-focus guards. Isolated child-fixture test executions are not double counted as additional top-level coverage.

## RED evidence and corrected attempts

Actual behavioral RED was observed before fixes: bridge 2PASS/1FAIL (never exits with unread stdout), runtime TTL 21PASS/3FAIL, renderer stale row 1FAIL, App dirty-history 2FAIL, archive 1PASS/4FAIL, automatic retry 1FAIL, original file click 1PASS/2FAIL, remote resolver skeleton 1PASS/5FAIL, raw punctuation helper 1FAIL, queued-open cancellation 1FAIL, wrapped remote parent 1FAIL, attached-focus dispatch 13PASS/1FAIL. Corrected source passed subsequent focused/full tests as recorded above.

Non-behavioral failures were retained rather than counted as RED: initial bridge borrow error; an oversized RPC fixture corrected to the real 256KiB service limit; four older Owner test literals missing the new resume policy field; and the new RPC collision fixture's initial `Harness.run()` demanding that a continuously repainting pane-border fade become idle in four steps. The fixture now uses an explicit two-frame settle and retains all real pointer/RPC ownership assertions. The first strict Clippy run rejected two nonminimal boolean expressions; equivalent `is_none_or` forms resolved them. No assertions, product checks or lints were disabled. Final source proof verifies all 19 changed/new Rust files against their recorded SHA256 values; all 42 evidence files were independently checked for byte length/digest and gzip decompression. Final `git diff --check` passed.

## Limits

No physical Mac sleep/wake test, live user-server failure injection, current Linux architecture build/run, native GPU/manual terminal interaction, or extended RSS/resource soak was performed in this task. The executed App pointer tests use actual offscreen egui interaction; remote path resolver tests use production provider validation and owned service replies, while existing SSH transport/runtime tests cover framed execution separately. This does not claim a live cloud/user-agent E2E run.
