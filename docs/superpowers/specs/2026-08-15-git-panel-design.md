# Git 패널 (orca 스타일) 설계

날짜: 2026-08-15
상태: 사용자 설계 승인 완료 (A안), 스펙 검토 대기
롤백 지점: 태그 `pre-git-panel-2026-08-15` (= main `4ad0cf4`, origin에 푸시됨)
참조: stablyai/orca의 source-control 패널 (사용자 제공 스크린샷 2장 기준)

## 목적과 범위

사이드 메뉴의 Git 버튼을 눌렀을 때, 현재의 플로팅 텍스트 diff 창 대신 orca 스타일의
git 상태 패널을 사이드바 본문에 보여준다. 파일 행을 클릭하면 메인 영역(터미널 자리)에
그 파일의 diff를 실용형 뷰어로 연다.

**사용자 결정으로 확정된 범위:**

- **읽기 전용.** 커밋 메시지 입력, 스테이지/언스테이지, PR 만들기 버튼은 이번 범위에서
  뺀다(스크린샷에는 있으나 사용자가 읽기 전용을 선택). 스테이지·커밋은 터미널에서 한다.
- 행 클릭 → 메인 영역에 해당 파일 diff (orca 두 번째 스크린샷의 동작).
- diff 뷰어는 **실용형**: 행 번호 + hunk 색 배경 + 접힌 문맥 표시 + hunk 이동.
  문법 강조·단어 단위 하이라이트·미니맵은 다음 단계로 미룬다.

## 1. 배치와 진입

- `SidebarTool::Git`(file_tree.rs:372)을 `Files`/`Notes`와 같은 **인라인 탭**으로 승격한다.
  `selected_tool == Git`이면 사이드바 본문이 git 패널이 된다 — Notes가 본문을 교체하는
  기존 패턴(file_tree.rs:2523)을 그대로 따른다.
- 기존 동작(`SidebarTool::Git → SidebarAction::ShowFocusedDiff` → 플로팅 `egui::Window`,
  diff_panel.rs:864)은 은퇴한다.
- 메인 영역 diff 뷰어는 `AgentTerminalView`(agent_terminal.rs:58)에 `Diff` 변형을
  추가해 연다 — `Home`/`Fleet`이 터미널 자리를 전면 교체하는 검증된 패턴.
  터미널 복귀는 기존 뷰 전환 UI를 쓴다.
- 파일 트리의 기존 `ShowDiff{..}` 진입점(file_tree.rs:2233)은 새 뷰어로 라우팅해
  기존 기능 회귀를 막는다.
- 기존 `DiffPanelUi`(UI 부분)는 새 두 모듈이 안정된 뒤 제거한다(전환 커밋에서 함께).
  단 `diff_panel.rs`의 off-thread 실행 유틸(`execute_io`와 git 수집 헬퍼)은 신규 수집
  코드의 기반으로 **이관**한다 — 버리는 것은 플로팅 창 UI뿐이다.

> **[2026-08-15 구현 중 정정] 플로팅 창은 유지한다.**
> 구현 단계에서 `DiffPanelUi::open_for_path`를 **작업 이력의 「변경 보기」**가 계속
> 쓰고 있는 것이 확인됐다(app.rs의 work-history 행 핸들러. `ui/workspace.rs`의
> `diff_window_id()` z-order 검사 2곳도 그 창에 딸려 있다). 이 기능은 이번 스펙의
> 범위 밖이라 창을 지우면 살아 있는 기능이 깨진다.
>
> 따라서 이번 범위에서 새 UI로 옮기는 진입점은 **사이드바 Git 탭**과 **세션 우클릭
> 「변경 보기」** 둘뿐이고, `diff_panel.rs`와 그 창은 그대로 둔다. repo 전체 진입점
> `open_for`는 프로덕션 호출자를 잃었지만, 그 파일 테스트 6곳이 그것으로 repo 전체
> 수집 경로(status+staged+unstaged+untracked 병합, 상한)를 커버하고 남은
> `open_for_path`는 경로 한정이라 대체하지 못하므로 제거하지 않고 문서화한다.
>
> **완전 은퇴 조건**: work-history의 diff를 `diff_viewer.rs`로 라우팅하는 후속 작업이
> 끝나면 그때 `diff_panel.rs`와 `diff_window_id()` 참조를 함께 제거한다.

## 2. 데이터 모델 (전부 유계)

```rust
pub struct GitPanelSnapshot {
    pub repo_root: PathBuf,
    pub branch: String,              // detached면 짧은 SHA
    pub upstream: Option<String>,    // 추적 브랜치, 없으면 origin/HEAD, 그것도 없으면 None
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<GitFileRow>,    // 워킹 트리 변경 (staged+unstaged+untracked 합산 뷰)
    pub committed: Vec<GitFileRow>,  // merge-base(upstream)..HEAD 에서 바뀐 파일
    pub changes_truncated: bool,
    pub committed_truncated: bool,
}

pub struct GitFileRow {
    pub rel_path: String,            // repo_root 기준. 렌더 시 파일명/디렉터리 분리
    pub status: char,                // M/A/D/R/C/U/?
    pub adds: Option<u32>,           // None = 바이너리 또는 untracked(numstat 없음)
    pub dels: Option<u32>,
}

pub struct FileDiffView {
    pub rel_path: String,
    pub mode: DiffMode,              // Working | Branch  (헤더 라벨: working diff / branch diff)
    pub hunks: Vec<DiffHunk>,        // 각 hunk: 구/신 시작 행번호 + 라인들(문맥/추가/삭제)
    pub gaps: Vec<u32>,              // hunk 사이 접힌 행수 ("⋯ N행" 표시용)
    pub truncated: bool,
    pub binary: bool,
}
```

상한: 섹션당 수집 행 수 상한(기존 diff_panel의 파일 수 상한 관례 재사용), diff 바이트
상한은 기존 `MAX_DIFF_BYTES` 재사용. 초과는 잘림 플래그 + 기존 `diff.truncated` 문구.

## 3. 수집 (off-thread)

기존 `AppHostIoAction::Diff` 경로(app.rs:10514 → `diff_panel::execute_io`)를 확장한다.
새 요청 두 종: `PanelSnapshot`, `FileDiff { rel_path, mode }`. 실행은 전부
`git_cli::run_git_limited`(기존 타임아웃·바이트 상한).

- repo_root: 기존 `git_cli::repo_root(cwd)` — cwd는 지금 diff 패널과 같은 활성 세션 cwd.
- 브랜치: `rev-parse --abbrev-ref HEAD` (`HEAD`가 나오면 detached → `rev-parse --short HEAD`)
- 업스트림: `rev-parse --abbrev-ref --symbolic-full-name @{u}`; 실패 시
  `symbolic-ref refs/remotes/origin/HEAD` 폴백; 둘 다 없으면 `upstream=None`이고
  ahead/behind와 committed 섹션을 숨긴다.
- ↑↓: `rev-list --left-right --count {upstream}...HEAD`
- 변경 사항: `status --porcelain -z -uall` (행 목록·status 문자) +
  `diff --numstat HEAD` (+/− 수치)를 경로로 병합. rename(R)은 porcelain의 새 경로 기준.
  porcelain XY 두 글자 중 표시 문자는 **워킹트리(Y) 우선, Y가 공백이면 X** — 한 파일이
  staged+unstaged 둘 다인 경우의 모호함을 없앤다. untracked(`??`)는 `?` 하나로 표시.
- COMMIT 됨: `merge-base {upstream} HEAD` → `diff --numstat -z {base}..HEAD` +
  `diff --name-status -z {base}..HEAD` 병합.
- 파일 diff: Working 모드 `diff --no-ext-diff HEAD -- {path}`
  (untracked는 파일 내용을 전량 추가로 합성, 바이트 상한 적용);
  Branch 모드 `diff --no-ext-diff {base}..HEAD -- {path}`.

갱신 시점: Git 탭 선택 시, 수동 새로고침 버튼, 파일 diff를 열 때(해당 파일 재수집).
자동 폴링은 두지 않는다 — 최소 자원 원칙. in-flight는 기존 Diff IO와 같은 latest-only.

## 4. 사이드 패널 UI (`crates/app/src/ui/git_panel.rs`, 신규)

스크린샷 레이아웃에서 읽기 전용 요소만:

```
feat/session-row-dot-first          ← 브랜치명 (한 줄, 말줄임)
→ origin/main        ↑4 ↓73  ↗     ← upstream · ahead(초록)/behind(빨강) · GitHub 열기
──────────────────────────────
∨ 변경 사항 9                모두 보기
  [icon] app.rs  crates/app/src         +3      M
  ...
∨ 브랜치에 COMMIT 됨 6        모두 보기
  [icon] file_tree.rs  crates/app/src/ui  +456 −221  M
```

- 행: 파일 아이콘 · 파일명(본문색) · 디렉터리(회색, 말줄임) · `+n`(초록) `−m`(빨강) ·
  상태 문자(M 등). 바이너리/untracked는 수치 생략.
- 행 클릭 → `SidebarAction::ShowFileDiff { rel_path, mode }` 반환(leaf는 intent만,
  App이 소유 — 저장소 관례).
- "모두 보기": 표시 상한(기본 접힘 시 섹션당 ~10행)을 넘을 때만 나타나는 펼침 토글.
- ↗ 클릭 → upstream이 GitHub remote면 브랜치 페이지 URL을 기존 open_url 경로로 연다.
  remote가 GitHub가 아니면 아이콘을 숨긴다.
- 섹션 접기(∨)는 섹션 헤더 클릭.

## 5. diff 뷰어 (`crates/app/src/ui/diff_viewer.rs`, 신규)

- 헤더: repo 상대 경로 + 모드 라벨("working diff" / "branch diff") + hunk ↑↓ 버튼 +
  터미널 복귀.
- 본문: hunk 단위 렌더 — 구/신 행번호 2열(모노), 추가 행 초록 배경, 삭제 행 빨강 배경,
  문맥 행 기본 배경. hunk 사이 접힌 구간은 "⋯ N행" 한 줄(이번 범위에서 클릭 펼침 없음).
- hunk ↑↓: 스크롤을 이전/다음 hunk 시작으로 이동.
- 바이너리: "바이너리 파일" 한 줄. 잘림: 기존 `diff.truncated` 문구.
- 렌더는 보이는 범위만(`ScrollArea::show_rows` 관례) — 대형 diff에서도 프레임 비용 유계.

## 6. 에러·유계·i18n

- cwd 미감지/repo 아님: 기존 `diff.no_cwd` 계열 문구 재사용. 변경 없음: `diff.clean`.
- git 호출 실패는 섹션 단위로 표시하고 패널 전체를 죽이지 않는다(헤더 실패 ≠ 목록 실패).
- 신규 문구 키는 **5로케일 전부**(en-US, ko-KR, ja-JP, zh-Hans, zh-Hant)에 추가.
- 모든 수집은 기존 timeout·바이트·행수 상한 안. 스냅샷/뷰 구조체는 위 상한으로 유계.

## 7. 테스트

- 파서 단위: porcelain -z, numstat, name-status, rev-list count, upstream 폴백 사슬.
- diff 파서 단위: hunk 분해, gap 계산, 바이너리/잘림 플래그.
- kittest: 행 클릭 → `ShowFileDiff` intent 방출, Git 탭 선택 → 패널 본문 전환,
  "모두 보기" 토글.
- 통합: 임시 repo 픽스처(기존 diff_panel 테스트의 `git init` 패턴, diff_panel.rs:1271)로
  스냅샷 수집 end-to-end.
- 게이트: `cargo test -p deppy-sijo` 전체 + clippy `-D warnings` 0건.
  UI 확인은 CLAUDE.md 워크플로대로 빌드·재기동 후 화면으로.

## 범위 외 (다음 단계 후보)

- 스테이지/언스테이지·커밋 메시지 입력·커밋 실행·PR 만들기 버튼
- 문법 강조·단어 단위 인트라라인 하이라이트·우측 미니맵·접힌 문맥 클릭 펼침
- 파일별 스테이지/되돌리기 hover 액션
- 자동 새로고침(파일 감시 연동)
