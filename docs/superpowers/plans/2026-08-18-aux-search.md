# 보조 본문 검색 구현 계획

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 이력·Git 보조 본문이 활성일 때 검색 버튼과 ⌘F가 그 본문을 검색한다 —
좌측 목록은 필터, 우측 본문은 일치 강조 + ↑↓ 이동.

**Architecture:** 스펙(`docs/superpowers/specs/2026-08-18-aux-search-design.md`)이 계약이다.
새 leaf 모듈 `ui/aux_search.rs`가 상태·매칭·검색 바를 갖고, 네 소비자(work_history·git_panel·
transcript_viewer·diff_viewer)가 그 순수 함수를 쓴다. App이 상태를 소유하고 진입을 배선한다.

**작업 위치:** 워크트리 `/Users/jr/Desktop/projects/deppy-history` (브랜치 `feat/work-history-depth`).

---

## 고정 API — 모든 Task가 이 시그니처에 맞춘다 (Task 1이 만든다)

```rust
// crates/app/src/ui/aux_search.rs

/// 본문당 세는 일치 상한 — 거대 diff에서 매 프레임 전수 스캔하지 않는다.
pub const MAX_AUX_MATCHES: usize = 500;

#[derive(Default, Clone)]
pub struct AuxSearchState {
    pub query: String,
    pub open: bool,
    pub active: usize,
}

impl AuxSearchState {
    /// 검색 버튼·⌘F — 열려 있으면 닫고, 닫혀 있으면 연다.
    pub fn toggle(&mut self);
    pub fn close(&mut self);
    /// 탭이 바뀌거나 워크스페이스가 바뀌면 완전히 비운다.
    pub fn reset(&mut self);
    /// 질의가 비었으면 검색하지 않는 것과 같다.
    pub fn is_active(&self) -> bool;
}

/// 대소문자 무시 부분 문자열 — `haystack` 안 일치들의 **바이트 범위**.
/// `MAX_AUX_MATCHES`에서 멈추고, 멈췄으면 `truncated = true`.
pub struct Matches {
    pub ranges: Vec<std::ops::Range<usize>>,
    pub truncated: bool,
}
pub fn find_matches(haystack: &str, needle: &str) -> Matches;

/// 목록 필터용 — 하나라도 걸리는지만 본다(범위는 필요 없다).
pub fn contains_match(haystack: &str, needle: &str) -> bool;

/// 일치 구간만 배경을 달리한 LayoutJob. `active`는 그 본문 기준 활성 일치 인덱스이며,
/// 해당 구간만 `active_bg`로 칠한다(나머지는 `match_bg`).
pub fn highlighted_job(
    text: &str,
    matches: &Matches,
    active: Option<usize>,
    base: egui::TextFormat,
    match_bg: egui::Color32,
    active_bg: egui::Color32,
) -> egui::text::LayoutJob;

/// 검색 바 한 줄. 반환 intent는 App이 소비한다.
pub enum AuxSearchAction { QueryChanged(String), Prev, Next, Close }
pub fn search_bar(
    ui: &mut egui::Ui,
    state: &AuxSearchState,
    total: usize,
    truncated: bool,
    catalog: &i18n::Catalog,
) -> Option<AuxSearchAction>;
```

**유니코드**: `find_matches`는 바이트 범위를 돌려주되 **항상 char 경계**에 맞춘다
(`to_lowercase()` 길이가 원문과 달라질 수 있으므로 소문자 사본에서 찾은 위치를 원문
인덱스로 되돌릴 때 주의). 한글·이모지에서 패닉하거나 글자를 반 토막 내면 안 된다.

---

### Task 1: `aux_search.rs` — 상태·매칭·검색 바

**Files:** Create `crates/app/src/ui/aux_search.rs`, Modify `crates/app/src/ui/mod.rs`

- [ ] **Step 1: 실패하는 테스트**

```rust
    #[test]
    fn 매칭은_대소문자를_무시한다() {
        let m = find_matches("Hello World", "world");
        assert_eq!(m.ranges, vec![6..11]);
        assert!(!m.truncated);
    }

    #[test]
    fn 빈_질의는_검색하지_않는다() {
        assert!(find_matches("아무거나", "").ranges.is_empty());
        assert!(!contains_match("아무거나", ""));
    }

    #[test]
    fn 매칭은_한글_경계를_지킨다() {
        let m = find_matches("가나다라", "나다");
        assert_eq!(m.ranges.len(), 1);
        let r = m.ranges[0].clone();
        assert_eq!(&"가나다라"[r], "나다", "char 경계에서 잘려야 한다");
    }

    #[test]
    fn 매칭은_상한에서_멈춘다() {
        let text = "a".repeat(MAX_AUX_MATCHES + 50);
        let m = find_matches(&text, "a");
        assert_eq!(m.ranges.len(), MAX_AUX_MATCHES);
        assert!(m.truncated);
    }

    #[test]
    fn 활성_일치만_다른_배경을_받는다() {
        let m = find_matches("aXaXa", "X");
        let job = highlighted_job("aXaXa", &m, Some(1), egui::TextFormat::default(),
            egui::Color32::RED, egui::Color32::GREEN);
        let bgs: Vec<_> = job.sections.iter().map(|s| s.format.background).collect();
        assert!(bgs.contains(&egui::Color32::GREEN), "활성 일치가 있어야 한다");
        assert_eq!(bgs.iter().filter(|c| **c == egui::Color32::GREEN).count(), 1);
    }
```

- [ ] **Step 2: 실패 확인** — `cargo test -p deppy-sijo --bin deppy-sijo ui::aux_search`
- [ ] **Step 3: 구현** — 위 고정 API대로. 검색 바는 이 저장소의 기존 입력 관례를 따르고,
      색은 `designall::tokens`에서만 고른다.
- [ ] **Step 4: 통과 확인**
- [ ] **Step 5: 커밋** — `git commit -m "feat(app): 보조 본문 검색 상태·매칭·검색 바" -- crates/app/src/ui/aux_search.rs crates/app/src/ui/mod.rs`

---

### Task 2: Git 파일 목록 필터

**Files:** `crates/app/src/ui/git_panel.rs`

`GitPanelUi::render`에 `filter: &str` 인자를 더한다(빈 문자열 = 필터 없음).
`changes`/`committed` 행을 `aux_search::contains_match(rel_path, filter)`로 거른다.
**워크트리 섹션은 필터하지 않는다**(스펙). 걸린 게 없으면 `search.no_match` 한 줄.

테스트: 필터가 걸리는 행 수, 빈 필터는 전부, 워크트리는 그대로.

커밋: `-- crates/app/src/ui/git_panel.rs`

---

### Task 3: 이력 카드 목록 필터

**Files:** `crates/app/src/ui/work_history.rs`

`WorkHistoryUi::show`에 `filter: &str`를 더한다. 카드는 `instruction`·`agent_summary`·
`messages_json`(파싱된 메시지 텍스트) 중 하나라도 걸리면 남긴다. 없으면 `search.no_match`.

테스트: 세 필드 각각으로 걸리는지, 빈 필터, 대소문자 무시.

커밋: `-- crates/app/src/ui/work_history.rs`

---

### Task 4: diff 본문 강조·이동

**Files:** `crates/app/src/ui/diff_viewer.rs`

`render`에 `search: Option<&AuxSearchView>` 같은 형태로 질의와 활성 일치를 받는다
(정확한 형태는 Task 1의 API에 맞춰 정하고 보고해라). 각 diff 행 텍스트에서 일치를 찾아
`highlighted_job`으로 그린다. 활성 일치가 있는 행으로 스크롤한다 — hunk 이동이 쓰는
`scroll_to_row`/`scroll_row_pitch` 관례를 재사용한다.

**행 수 세기**: 본문 전체 일치 수는 App이 알아야 카운터를 그린다. `render`가 그 프레임에
센 총 일치 수를 반환하거나, 순수 함수로 미리 세는 쪽 중 **매 프레임 전수 스캔을 피하는**
방식을 골라라(질의가 바뀔 때만 세는 캐시 권장). 고른 이유를 보고해라.

커밋: `-- crates/app/src/ui/diff_viewer.rs`

---

### Task 5: 원문 본문 강조·이동

**Files:** `crates/app/src/ui/transcript_viewer.rs`

Task 4와 같은 규칙. 메시지 텍스트에서 일치를 찾아 강조하고, 활성 일치가 있는 메시지로
스크롤한다.

**주의**: 이 파일은 **측정 높이 가상화**로 바뀌는 중이다(별도 작업). 그 커밋이 먼저 들어간
뒤에 시작하고, 그 구조(높이 캐시·누적합) 위에서 동작하게 해라 — 강조가 높이를 바꾸지
않으므로 캐시를 무효화할 필요는 없지만, 활성 일치로의 스크롤은 그 구조의 오프셋 계산을
그대로 써야 정확하다.

커밋: `-- crates/app/src/ui/transcript_viewer.rs`

---

### Task 6: App 배선 + 진입 + i18n

**Files:** `crates/app/src/app.rs`, `crates/app/src/ui/workspace.rs`,
`crates/i18n/locales/*/messages.txt`

- App에 `aux_search: ui::aux_search::AuxSearchState`. 활성 보조 탭이 바뀌거나 워크스페이스가
  바뀌면 `reset()`(후자는 `reset_git_surfaces` 옆).
- `render_git_tab_body`/`render_work_history_tab_body` 상단에 `search_bar`를 그리고, 남은
  rect를 기존 마스터-디테일에 넘긴다.
- **검색 버튼**: `workspace.rs`의 툴바 Search가 보조 본문 활성일 때는 터미널 검색 대신
  intent를 올리게 한다. 지금은 `input_enabled`가 false라 아무 일도 안 하는데, 그 게이트를
  건드리지 말고 **보조 탭이 활성이면 별도 intent**를 올리는 경로를 더해라(입력 소유권
  fail-closed 계약은 유지).
- **⌘F**: `app.rs`의 `A::TerminalSearch => self.active.workspace_ui.open_search()` 한 곳에서
  분기한다 — 보조 본문 활성이면 `aux_search.toggle()`.
- **Esc**: 검색 바가 열려 있으면 닫는다.
- i18n 7키를 5로케일에 추가(`search.placeholder`, `search.no_match`, `search.count`,
  `search.prev`, `search.next`, `search.close`, `search.truncated`).

테스트: 보조 본문 **비활성**이면 검색 버튼·⌘F가 기존 터미널 검색을 연다는 계약(회귀 방지),
탭 전환 시 `reset`.

커밋: 경로 명시.

---

## 게이트 (모든 Task 공통)

1. `cargo test -p deppy-sijo` 전부 통과
2. `cargo clippy --workspace --all-targets -- -D warnings` 경고 0
3. `cargo run -q -p xtask -- check-boundary` 통과
4. i18n을 건드렸으면 `cargo test -p i18n`

## 자체 검토

- 순서 제약: Task 1이 API를 정의하므로 먼저. Task 2·3·4는 그 뒤 병렬(파일 무관).
  Task 5는 가상화 커밋 뒤. Task 6이 마지막(모든 소비자 시그니처가 정해진 뒤).
- 스펙 커버리지: 매칭 규칙·상한 → Task 1, 좌측 필터 → 2·3, 우측 강조 → 4·5, 진입·i18n → 6.
