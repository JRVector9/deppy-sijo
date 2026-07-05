# PR-R05 Findings

## Summary
- 전체 판정: Pass with Issues
- Critical: 0
- High: 1
- Medium: 5
- Low: 0

기존 terminal crate 테스트는 bracketed paste, 한글 wide char 2셀 렌더링, NFD 한글 합성, IME commit, ASCII selection copy를 커버한다. 다만 CJK wide-char selection endpoint, DnD bracketed paste 경로, multi-scalar grapheme/emoji cluster, terminal internal drag/drop paste contract, required CJK/emoji fixtures, clipboard failure handling이 부족하다.

## Scope Reviewed
- 검토한 파일/모듈: `crates/terminal/src/{input_mapper,renderer_egui,alacritty_backend,viewport_snapshot,backend}.rs`, `crates/app/src/ui/{workspace,file_tree}.rs`, `crates/app/src/app.rs`, `crates/runtime/src/{in_process,command,event}.rs`, `crates/session/src/session.rs`
- 실행한 명령: `cargo check --workspace --all-targets` pass, `cargo test --workspace --no-run` pass, PR-R05 `rg` 검색, `cargo test -p terminal` pass
- 확인한 테스트: terminal crate 24 passed. Japanese/Chinese/emoji path roundtrip, clipboard failure, GUI IME smoke는 자동 테스트로 확인되지 않음.

## Findings

### Finding 1
Severity: High
Area: Terminal selection / CJK wide char boundary
Files: `crates/app/src/ui/workspace.rs`, `crates/terminal/src/{renderer_egui,alacritty_backend}.rs`
Evidence: pointer hit-test가 raw cell index를 저장한다. renderer와 `selection_text()`는 `wide_spacer`를 skip하지만 selection endpoint를 normalize하지 않는다. CJK wide char 오른쪽 절반에서 drag를 시작하면 selection이 spacer cell에서 시작할 수 있다.
Why it matters: Japanese/Chinese/Korean path/text copy에서 첫 글자가 빠질 수 있다.
Reproduction: `가A`를 렌더하고 `가` 오른쪽 절반에서 `A`까지 drag하면 selection cell `1..=2`가 되어 `selection_text()`가 `A`만 반환할 수 있다.
Suggested fix: spacer endpoint를 owning wide char leading cell로 확장하는 shared selection normalization helper를 render/copy 전에 적용한다.
Suggested test: `TerminalViewportSnapshot`에 wide cell + spacer를 만들고 양쪽 half에서 시작/종료한 selection이 전체 문자를 복사하는지 검증한다.

### Finding 2
Severity: Medium
Area: Drag/drop paste / bracketed paste
Files: `crates/terminal/src/input_mapper.rs`, `crates/app/src/ui/workspace.rs`, `crates/app/src/app.rs`
Evidence: clipboard paste는 `input_mapper::map_event()`를 통해 bracketed paste로 wrap되지만 file tree drop과 path insert는 raw quoted bytes를 `RuntimeCommand::WriteInput`으로 직접 보낸다.
Why it matters: bracketed-paste-aware shell/editor/TUI에서 dropped paths가 paste payload가 아니라 typed keystrokes처럼 처리된다.
Reproduction: bracketed paste가 켜진 app에서 `프로젝트/설정파일.rs` 또는 `project/🚀-deploy/config.json`을 drop한다.
Suggested fix: clipboard paste, file drop, path insert가 같은 paste byte helper를 사용하게 하고 session의 current `bracketed_paste` state를 반영한다. no-auto-Enter는 유지한다.
Suggested test: required strings에 대해 bracketed on/off byte generation과 DnD path insertion wrapper를 테스트한다.

### Finding 3
Severity: Medium
Area: Grapheme boundary / emoji path coverage
Files: `crates/terminal/src/{viewport_snapshot,alacritty_backend}.rs`, `crates/terminal/Cargo.toml`
Evidence: `TerminalCell`은 단일 `char`를 저장한다. `composed_char()`는 NFC가 정확히 한 scalar를 만들 때만 zerowidth sequence를 보존한다. grapheme segmentation dependency/test가 없다.
Why it matters: required fixture `project/🚀-deploy/config.json`의 rocket은 단일 scalar라 현재 모델로 표현 가능하지만, variation selector/ZWJ emoji sequence는 copy-preserving unit으로 표현되지 않는다.
Reproduction: `emoji/☺️.txt`, `emoji/👩‍💻.rs`를 feed하고 snapshot/copy output을 확인한다.
Suggested fix: v0 limit을 명시하거나 snapshot/copy data에 grapheme/zerowidth sequence를 보존한다.
Suggested test: required strings + VS16 + ZWJ emoji fixture.

### Finding 4
Severity: Medium
Area: Clipboard abstraction / failure handling
Files: `crates/platform/src/lib.rs`, `crates/app/src/ui/{workspace,file_tree}.rs`
Evidence: copy는 `ui.ctx().copy_text(...)`를 직접 사용한다. `Result`를 반환하는 clipboard abstraction이 없고 `platform`은 Clipboard를 later PR로 남긴다.
Why it matters: Wayland/headless/portal failure 같은 clipboard 실패가 silent이다.
Reproduction: native clipboard 접근이 실패하는 환경에서 terminal text를 선택 후 copy한다.
Suggested fix: narrow platform clipboard API 또는 notification bridge로 copy failure를 surface한다.
Suggested test: mock clipboard failure test로 localized notification/error 발생 검증.

### Finding 5
Severity: Medium
Area: Terminal internal drag/drop paste ambiguity
Files: `crates/app/src/ui/workspace.rs`, `crates/terminal/src/renderer_egui.rs`
Evidence: terminal drag is used for selection indexes, but the only drop payload handled by workspace terminal drop is `PathBuf` from the file tree/sidebar. No terminal selected-text drag source/drop paste path was found.
Why it matters: v3.1 and the prompt pack mention terminal internal drag/drop paste, while current implementation supports file-tree path DnD into terminal. These are different UX contracts and test surfaces.
Reproduction: select text in a terminal and try to drag/drop it into the same or another terminal pane; no selected-text DnD paste path is present.
Suggested fix: Define whether "terminal internal DnD paste" means selected terminal text drag/drop or file-tree path DnD. If selected-text DnD is required, route it through the same paste byte helper as clipboard/path DnD.
Suggested test: selected terminal text DnD either no-ops by explicit contract or inserts the selected text with bracketed paste semantics and no auto-execute.

### Finding 6
Severity: Medium
Area: Required CJK/emoji fixture coverage
Files: `crates/terminal/*`, `crates/app/src/ui/workspace.rs`
Evidence: required PR-R05 strings are present in the prompt/review docs but not as executable tests. Existing tests cover Korean wide char, NFD Korean, IME commit, bracketed paste, and ASCII selection, but not the required Japanese, Simplified Chinese, Traditional Chinese, Korean path, and emoji path fixture set end to end.
Why it matters: These fixtures are the explicit acceptance surface for selection/copy and paste/DnD byte generation across CJK and emoji paths.
Reproduction: `rg "プロジェクト|项目|專案|프로젝트|🚀-deploy" crates -g '*.rs'` does not find executable fixture tests.
Suggested fix: Add a mandatory fixture suite for `src/main.rs`, `プロジェクト/設定ファイル.rs`, `项目/配置文件.rs`, `專案/設定檔.rs`, `프로젝트/설정파일.rs`, and `project/🚀-deploy/config.json`.
Suggested test: selection/copy, clipboard paste, file-tree/path DnD byte generation, bracketed on/off, and no-auto-Enter assertions for all required strings.

## Second Pass Update
- Finding 3 wording is narrowed: evidence supports multi-scalar grapheme/VS16/ZWJ preservation risk, not failure of every emoji path.
- Added Finding 5 to separate terminal-selected-text internal DnD paste from file-tree `PathBuf` DnD.
- Added Finding 6 to make required CJK/emoji fixtures an explicit coverage finding rather than only suggested tests.

## Regression Risks
- DnD paste fix는 dropped path 뒤에 자동 Enter를 추가하면 안 된다.
- selection normalization은 ASCII selection, row trimming, existing CJK rendering을 깨지 않아야 한다.
- clipboard abstraction은 runtime/session/persistence를 끌어들이지 않아야 한다.

## Recommended Build PRs
- PR-B04a: wide char selection normalization and CJK copy tests
- PR-B04b: unified paste helper for clipboard/path insert/DnD with bracketed paste
- PR-B04c: clipboard abstraction with failure notification
- PR-B04d: mandatory PR-R05 fixture suite

## Open Questions
- drag/drop path insertion도 target session이 bracketed paste mode이면 항상 bracketed paste로 처리할 것인가?
- terminal snapshot model은 copy 전용 grapheme cluster 보존을 지원해야 하는가?
