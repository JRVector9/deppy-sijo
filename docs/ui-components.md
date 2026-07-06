# Deppy Sijo UI 컴포넌트 디자인 룰

egui 위에서 UI를 일관되게 만들기 위한 규칙. 새 컴포넌트/화면은 이 규칙을 따른다.
공용 헬퍼는 `crates/app/src/ui/components.rs`에 있고, 색은 팔레트(`theme.rs`)에서 온다.

## 1. 색 (팔레트 — `ui.visuals()`에서 참조, 하드코딩 금지)

| 용도 | 소스 | 다크값 |
|---|---|---|
| 액센트 | `visuals().selection.bg_fill` | `#43b8cd` |
| 액센트-소프트(선택/hover 배경) | `accent.gamma_multiply(0.15)` | 반투명 시안 |
| 본문 텍스트 | `visuals().text_color()` | `#d7d8db` |
| 흐린 텍스트(힌트/보조) | `visuals().weak_text_color()` | `#8b8f98` |
| 패널 배경 | `visuals().panel_fill` | `#1b1b21` |
| 컴포넌트 배경(입력/버튼) | `visuals().faint_bg_color` / `widgets.inactive.bg_fill` | `#22222a` |
| hover 배경 | `widgets.hovered.weak_bg_fill` | `#2a2a33` |
| 경계선(hairline) | `widgets.noninteractive.bg_stroke` | `#3a3a42` |
| 터미널 배경 | 고정 `#18181c` (테마 무관 항상 다크) | — |

- 예외: 터미널 pane·pane 헤더는 **테마 무관 항상 다크**라 고정 RGB를 쓴다.
- 상태 색(세션 레일/점): 실행=`#43b8cd` 대기=`#d9b26a` 승인=`#e0a83e` 완료=`#6cc26c` 오류=`#e05c53` 유휴=accent.

## 2. 크기 (통일)

- **인터랙티브 컨트롤 높이**: **30px** (버튼·스텝퍼·드롭다운·토글 그룹).
- **아이콘 셀**: **18×18** painter 셀, 도형은 셀 중앙 정렬(이모지 금지 — 폰트에 없어 □ 깨짐).
- **모서리 라운딩**: 컨트롤 6, 세그먼트/네비 7.
- **경계선**: 1px, hairline 색.

## 3. 폰트 크기

| 요소 | 크기 |
|---|---|
| 섹션 제목 | 16 (strong) |
| 행 라벨 | 13.5 |
| 힌트/설명(보조) | **11.5 (최소)** |
| 컨트롤 내 값/라벨 | 13 |
| 네비 항목 | 13.5 |

## 4. 정렬

- **컴포넌트 내 텍스트는 세로·가로 중앙 정렬을 기본**으로 한다 (버튼·스텝퍼·드롭다운 값·세그먼트).
- 행(row) 라벨은 좌측, 컨트롤은 우측(`Layout::right_to_left`).
- 아이콘은 셀 중앙.

## 5. 공용 컴포넌트 (`components.rs`)

- `action_button(ui, label) -> bool` — 액센트-소프트 배경, accent 텍스트, 높이 30, 중앙 정렬.
- `text_input(ui, buf, hint)` — 컨트롤 배경, margin 대칭, 높이 30, 다른 입력과 동일.
- `stepper` / `dropdown` / `toggle_switch` / `segmented` — settings.rs (값 입력용, 위 규칙 준수).

관리/모니터 패널(자격증명·연결·환경·에이전트)의 버튼·입력은 위 공용 컴포넌트를 쓴다.

## 6. 레이아웃 여백

- 창/패널 여백: 컨텐츠 좌우 여백은 최소로. 구분선(hairline)은 `hairline_full`로 **패널 경계까지** 긋는다.
- 작업창(터미널)은 여백 0, pane 사이 1px 구분선만.
