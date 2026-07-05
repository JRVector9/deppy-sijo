# PR-R04 Findings

## Summary
- 전체 판정: Block
- Critical: 0
- High: 2
- Medium: 4
- Low: 0
- 확인된 정상 동작: terminal drop/context insert 경로는 `WriteInput`만 보내며 Enter/newline 자동 전송은 보이지 않는다.

## Scope Reviewed
- 검토한 파일/모듈: `crates/app/src/ui/file_tree.rs`, `crates/app/src/ui/workspace.rs`, `crates/app/src/app.rs`, `crates/app/src/config.rs`, `crates/terminal/src/input_mapper.rs`, `docs/file-tree-design.md`
- 실행한 명령: `cargo check --workspace --all-targets` pass, `cargo test --workspace --no-run` pass, `rg "file_tree|folder|drag|drop|quote|paste" crates/app crates/runtime crates/terminal`
- 확인한 테스트: 테스트 바이너리 빌드까지만 확인. 100k 파일 수동/성능 smoke와 OS별 DnD smoke는 수행하지 않음.

## Findings

### Finding 1
Severity: High
Area: Shell-specific path quoting
Files: `crates/app/src/ui/file_tree.rs`, `crates/app/src/ui/workspace.rs`, `crates/app/src/app.rs`
Evidence: PR-R04는 PowerShell/cmd/bash/zsh/fish quoting을 요구하지만 `shell_quote`는 POSIX single-quote 방식 하나만 구현하고 모든 terminal drop/context insert 경로가 이 함수만 사용한다.
Why it matters: `cmd.exe`는 single quote를 quoting으로 보지 않고 PowerShell의 single quote escape는 `''`라서 POSIX `'\\''`가 깨진다.
Reproduction: PowerShell 또는 cmd 세션에서 `C:\Users\me\My File's.txt`를 드롭하면 하나의 path token으로 해석되지 않는다.
Suggested fix: session shell family를 runtime/session metadata로 노출하거나 platform default를 사용해 POSIX, PowerShell, cmd별 quote 함수를 분리한다.
Suggested test: bash/zsh/fish/PowerShell/cmd별 table test와 space, apostrophe, backslash, drive colon, CJK/emoji path 검증.

### Finding 2
Severity: High
Area: Large workspace freeze / lazy loading boundary
Files: `crates/app/src/ui/file_tree.rs`
Evidence: `set_root()`가 UI 흐름에서 즉시 `refresh()`를 호출하고 `refresh()`/`toggle_dir()`는 `read_children()`를 동기 실행한다. `read_children()`는 해당 디렉터리 모든 entry를 읽고 전체 sort한다. `show_rows`는 렌더만 가상화한다.
Why it matters: 재귀 eager load는 아니지만 root나 펼친 단일 디렉터리에 100k 파일이 있으면 UI thread가 enumeration/sort 동안 멈춘다.
Reproduction: workspace root에 100k 파일을 만들고 해당 workspace를 열거나 새로고침/확장을 누른다.
Suggested fix: directory listing을 cancellable background task로 옮기고 root/dir별 generation token으로 stale 결과를 버린다. 필요하면 chunked append, entry cap + more sentinel을 둔다.
Suggested test: 100k direct-child tempdir smoke와 root 전환 중 stale listing 폐기 테스트.

### Finding 3
Severity: Medium
Area: Ignore rules
Files: `crates/app/src/ui/file_tree.rs`, `crates/app/src/app.rs`
Evidence: 구현된 ignore는 watcher용 app data prefix와 dotfile 숨김뿐이다. `.gitignore`, `.git/info/exclude`, global gitignore, `target/`, `node_modules/` 같은 rules를 `read_children()`에 적용하지 않는다.
Why it matters: 무시되어야 할 대형 디렉터리가 트리에 표시/나열되어 freeze 위험도 커진다.
Reproduction: workspace `.gitignore`에 `target/` 또는 `node_modules/`를 추가해도 트리에 계속 표시된다.
Suggested fix: root별 ignore matcher를 구성해 listing, flatten, watcher event filtering에 같은 규칙을 적용한다.
Suggested test: nested `.gitignore`, negation, CJK/emoji 이름, `target/`/`node_modules/` fixture.

### Finding 4
Severity: Medium
Area: Multi-path drop / insert
Files: `crates/app/src/ui/file_tree.rs`, `crates/app/src/ui/workspace.rs`
Evidence: DnD payload type이 단일 `PathBuf`이다. `Vec<PathBuf>`나 multi-path payload가 없다.
Why it matters: v3.1/prompt-pack hardening 범위에서는 여러 파일을 한 번에 terminal command argument로 넣는 UX가 필요하다. 다만 `docs/file-tree-design.md`는 multi-select drag를 non-goal로 둔 적이 있으므로 현재 결함은 file-tree-design 위반이 아니라 문서 간 acceptance gap이다.
Reproduction: 여러 파일을 선택해 드롭하는 경로가 없다.
Suggested fix: `PathDropPayload { paths: Vec<PathBuf> }` 타입을 만들고 terminal insert는 shell별 quoted path를 space join한다.
Suggested test: 두 개 이상 path drop 시 `quoted(path1) + " " + quoted(path2)` 삽입 및 newline 없음 검증.

### Finding 5
Severity: Medium
Area: Drop contract ambiguity / tree-internal file move
Files: `crates/app/src/ui/file_tree.rs`, `docs/file-tree-design.md`
Evidence: PR-R04는 "drop은 기본적으로 path insert만"을 중점으로 두지만 구현은 terminal 위 drop은 path insert, tree header/folder row drop은 즉시 `start_move()`로 파일 이동이다. `docs/file-tree-design.md`는 tree-internal move를 목표로 적어 문서 간 계약이 충돌한다.
Why it matters: path insert gesture로 이해한 drag/drop이 트리 영역에서는 mutating file move가 된다.
Reproduction: 파일 트리에서 `a.txt`를 폴더 `b/` 위에 드롭하면 terminal insert가 아니라 파일 이동이 실행된다.
Suggested fix: "path insert only"가 terminal drop에만 적용되는지 명확히 하고, 전역 기본이라면 tree-internal move를 modifier/명시 모드/확인 UI 뒤로 보낸다.
Suggested test: terminal drop은 `WriteInput`만, tree drop은 확정 계약에 맞는 move 또는 no-op/confirm 테스트.

### Finding 6
Severity: Medium
Area: Folder tree root contract / unset workspace path
Files: `crates/app/src/app.rs`, `crates/app/src/ui/file_tree.rs`, `docs/file-tree-design.md`
Evidence: second pass found that `active_tree_root()` falls back to Desktop when `workspaces.path` is empty, while the folder tree design says empty path should show the unset-path guidance.
Why it matters: A workspace with no project path can silently expose Desktop as the DnD/move root, so file moves may operate outside the intended workspace.
Reproduction: create/select a workspace with empty path and inspect the folder tree root/drop behavior.
Suggested fix: Either document Desktop fallback as intended and gate mutating DnD carefully, or restore `None` for empty workspace path so the unset-path state is shown.
Suggested test: empty workspace path shows path-required state or explicitly tested Desktop fallback behavior, with terminal/tree DnD disabled or constrained according to the chosen contract.

## Second Pass Update
- PR-R04 findings 1, 2, 3, and 5 remain valid.
- Multi-path drop remains Medium, but is a v3.1/prompt-pack hardening gap rather than a `docs/file-tree-design.md` violation.
- Added Finding 6 for empty workspace path fallback to Desktop.

## Regression Risks
- shell-aware quoting이 기존 POSIX bash/zsh/Korean path test를 깨지 않아야 한다.
- async listing은 workspace 전환, collapse, refresh, watcher reload와 race가 생긴다.
- ignore rules 도입은 "show hidden"과 "show ignored" 정책 분리가 필요하다.

## Recommended Build PRs
- PR-B03-A: shell-aware quoting, multi-path payload, no-auto-Enter tests
- PR-B03-B: async/chunked folder listing, 100k file smoke, stale cancellation
- PR-B03-C: gitignore/ignore matcher integration
- PR-B03-D: tree-internal DnD move contract clarification

## Open Questions
- "drop은 기본적으로 path insert만"이 terminal drop에만 적용되는가?
- Runtime/session이 active shell family를 UI에 노출해야 하는가?
- context menu path insert도 terminal drop처럼 trailing space를 붙여야 하는가?
