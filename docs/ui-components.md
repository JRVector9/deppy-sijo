# Deppy Sijo UI 컴포넌트 가이드

최종 갱신: 2026-07-10

대상: `egui 0.35`, 설정 창의 인라인 상세 페이지

구현 기준: [`crates/app/src/ui/settings.rs`](../crates/app/src/ui/settings.rs),
[`crates/app/src/fonts.rs`](../crates/app/src/fonts.rs)

이 문서는 설정 화면을 추가하거나 수정할 때 참고하는 구현 규격이다. 설정 상세 화면에서는
이 문서의 치수와 컴포넌트를 우선하며, 화면마다 같은 값을 다시 하드코딩하지 않는다.

## 1. 기준 화면과 적용 범위

2026-07-10 일반 설정의 **모양** 화면을 기준으로 다음 인라인 페이지가 같은 컴포넌트 체계를 공유한다.

- 일반(모양)
- 언어
- Terminal
- Performance
- Remote (TLS)

환경/API, 자격증명, 연결, 에이전트, 워크스페이스처럼 자체 레이아웃을 가진 관리 화면에는
`SettingsDetailShell`을 중첩 적용하지 않는다.

디자인 참고 구현은
[`design/환경설정 메뉴 구성/src/components/SessionWorkspace.tsx`](../design/환경설정%20메뉴%20구성/src/components/SessionWorkspace.tsx)의
`GeneralSettingsPage`, `SettingsDetailShell`, `SettingsRow`, `SettingsSegmented`,
`SettingsSelect`, `SettingsToggle`, `SettingsStepper`다. 실제 앱의 최종 기준은 Rust 토큰과 테스트다.

## 2. 적용 폰트와 타이포그래피

macOS에서 UI 폰트를 따로 선택하지 않았을 때 실제 기본 폰트는 **AppleGothic**이다.
설정의 닫힌 선택 상자에도 `기본(자동)` 대신 현재 적용값인 `AppleGothic`을 표시한다.
팝업 목록에는 기본값 복귀용 `기본(자동)` 항목을 유지한다.

| 토큰 | 적용 위치 | 크기 |
|---|---|---:|
| `page_title` | 상세 페이지 제목 | 15px |
| `section_title` | Remote 등 내부 섹션 제목 | 14px |
| `row_title` | 설정 행 제목 | 14px |
| `row_description` | 행 설명·힌트 | 13px |
| `control` | 선택 상자·세그먼트·스텝퍼 값 | 14px |

- 구현 토큰: `SettingsTypography`, `SETTINGS_TYPE`
- UI 텍스트: `FontId::proportional`
- 페이지 제목: `strong_text_color` 사용
- 모노스페이스 값: 포트, 지문, 스텝퍼 숫자 등 값 성격이 분명한 곳에만 사용
- 터미널 pane 폰트는 별도 설정이며 이 표의 영향을 받지 않는다.
- 선택 폰트 경로의 표시명은 `fonts::effective_ui_font_name`으로 계산한다.

## 3. 공통 레이아웃 토큰

구현 토큰은 `SettingsDetailMetrics`, `SETTINGS_DETAIL`에 한 번만 정의한다.

| 항목 | 값 | 설명 |
|---|---:|---|
| 상세 좌우 여백 | 26px | 양쪽 동일 |
| 상세 상단 여백 | 20px | 네이티브 타이틀바 제외 본문 기준 |
| 상세 하단 여백 | 40px | 마지막 행 아래 스크롤 여유 |
| 설정 행 높이 | 68px | 구분선 포함 고정 높이 |
| 컨트롤 열 | 360px | 우측 정렬 |
| 열 사이 간격 | 20px | 텍스트와 컨트롤 사이 |
| 공통 컨트롤 높이 | 34px | 선택·세그먼트·스텝퍼 |
| 선택 상자 폭 | 230px | `SettingsSelect` |
| 세그먼트 한 칸 폭 | 104px | 테마는 총 312px |

좁은 창에서는 컨트롤 열을 `min(360, 사용 가능 폭 - 20)`으로 축소하고, 왼쪽 텍스트는
컨트롤 영역을 침범하지 않도록 clip한다. 구분선은 상세 내용 폭 안에서만 그리며 26px 여백을
가로질러 창 끝까지 확장하지 않는다.

## 4. 컴포넌트 목록

| 문서명 | Rust 구현 | 규격·역할 |
|---|---|---|
| `SettingsDetailShell` | `settings_detail_shell` | 배경, 26/20/40px 여백, 수직 간격 0을 제공하는 상세 surface |
| `SettingsPageTitle` | `page_title` | 15px 제목, 아래 16px 간격과 1px 구분선 |
| `SettingsRow` | `row` | 68px, `1fr + 360px`, 20px gap, 하단 구분선 |
| `SettingsSegmented` | `segmented` | 칸당 104×34px, 선택 배경과 painter 아이콘 |
| `SettingsSelect` | `settings_select_button` | 230×34px, 좌측 말줄임 라벨, 우측 삼각형 |
| `SettingsToggle` | `toggle_switch` | 46×26px, 18px knob, 3px 내부 여백, radius 2 |
| `SettingsStepper` | `stepper`, `stepper_f32` | 226×34px, `138px + 44px + 44px` |
| `SettingsHairline` | `settings_hairline` | 물리 픽셀 중심에 맞춘 내용 폭 1px 선 |

컴포넌트들은 현재 `settings.rs` 내부 전용이다. 다른 모듈에서도 같은 컴포넌트가 필요해지면
구현을 복사하지 말고 `ui/settings_components.rs`로 추출해 공유한다.

### SettingsDetailShell

- 인라인 상세 페이지에 한 번만 적용한다.
- 내부 행이 자체 높이와 선을 가지므로 `item_spacing.y = 0`이다.
- 상세 배경은 다크 모드 `#1a1a1a`다.
- 자체 레이아웃을 가진 관리 페이지에는 적용하지 않는다.

### SettingsRow

- 제목과 설명은 왼쪽, 컨트롤은 오른쪽 정렬이다.
- 설명이 있으면 제목 중심은 행 상단에서 21px, 설명 중심은 46px다.
- 설명이 없으면 제목은 행의 세로 중앙에 놓는다.
- 제목/설명은 우측 컨트롤 열 앞에서 clip한다.
- 행마다 하단 1px 선을 그린다.

### SettingsSelect

- 라벨은 중앙 정렬이 아니라 12px 왼쪽 padding 기준으로 정렬한다.
- 화살표는 글자 `▾` 대신 painter 삼각형을 사용한다. 폰트 누락에 의한 `□` 표시를 방지한다.
- 긴 라벨은 화살표 30px 앞에서 clip한다.
- 선택 팝업 최소 폭도 230px로 맞춘다.

### SettingsSegmented

- 선택 상태: `nav_active` 배경 + `accent` 텍스트/아이콘
- 비선택 상태: `surface` 배경 + `text_secondary` 텍스트/아이콘
- 시스템 `▣`, 라이트 `☀`, 다크 `●`는 이모지 문자가 아니라 painter 도형으로 그린다.
- 세 칸 외곽선과 칸 사이 구분선을 1px로 다시 그린다.

### SettingsToggle

- ON: accent 배경/테두리, 흰색 knob
- OFF: surface 배경, border 테두리, muted knob
- 클릭 시 값을 반전하고 변경 여부를 반환한다.

### SettingsStepper

- 값 영역 138px, 감소·증가 버튼 각각 44px다.
- 정수는 천 단위 쉼표를 표시한다.
- `f32` 값은 정수면 소수점을 숨기고, 필요할 때만 소수 첫째 자리까지 표시한다.
- 증감 후 반드시 설정 범위로 clamp한다.

## 5. 설정 창 색 토큰

색은 화면 구현에서 중복 하드코딩하지 않고 `apply_settings_palette`와 컴포넌트 색 헬퍼를 사용한다.

| 용도 | 다크 | 라이트 | 코드 소스 |
|---|---|---|---|
| 상세 배경/input | `#1a1a1a` | `#ffffff` | `extreme_bg_color` |
| 컨트롤 surface | `#242424` | `#f0f0f0` | `panel_fill` |
| hover | `#2c2c2c` | `#e8e8e8` | `widgets.hovered.bg_fill` |
| 관리 panel | `#202020` | `#fafafa` | `faint_bg_color` |
| 기본 border | `#3a3a3a` | `#c4c4c4` | noninteractive stroke |
| input border | `#404040` | `#b8b8b8` | `settings_input_border` |
| 본문 텍스트 | `#d4d4d4` | `#1a1a1a` | `text_color` |
| muted | `#717171` | `#888888` | `weak_text_color` |
| 행 설명 | `#aaaaaa` | `#444444` | `settings_text_secondary` |
| accent | `#4da6c8` | `#3a88bf` | `selection.bg_fill` |
| 선택 배경 | `#2e4a5e` | `#ccdeed` | `settings_nav_active` |
| focus border | `#5a9fd4` | `#3a88bf` | `selection.stroke` |

터미널 pane과 pane 헤더는 테마와 관계없이 항상 다크다. 설정 창의 라이트/다크 전환이
터미널 ANSI 색상이나 pane 배경을 바꾸면 안 된다.

## 6. 일반 설정(모양) 구성 순서

일반 페이지는 아래 순서를 유지한다.

1. 테마 — 시스템/라이트/다크 세그먼트
2. UI 폰트 — 실제 적용 폰트명이 보이는 선택 상자
3. 폴더 트리 사이드바 — 토글
4. 세션 자동 이어가기 — 토글
5. 에이전트 상태 hook — 토글

한국어 설명 문구의 기준은 다음과 같다.

- `창 크롬만 전환, 터미널 pane은 항상 다크`
- `사이드바·설정 등 UI 텍스트 폰트 (한글 지원 시스템 폰트)`
- `OFF면 Panel 미생성 — 리소스 0`
- `재시작 시 이전 claude/codex 세션을 resume 명령으로 자동 실행`
- `claude/codex 설정에 hook을 설치해 승인/입력 대기를 정확히 감지`

문구는 Rust에 직접 넣지 않고 `crates/i18n/locales/*/messages.txt`의 키를 사용한다.

## 7. 사용 예시

다음 패턴처럼 공통 제목·행·컨트롤을 조합한다.

```rust
fn example_page(ui: &mut egui::Ui, config: &mut Config, changed: &mut bool) {
    page_title(ui, "예시");
    row(
        ui,
        "기능 사용",
        Some("기능에 대한 짧고 구체적인 설명"),
        |ui| {
            if toggle_switch(ui, &mut config.ui.example_enabled) {
                *changed = true;
            }
        },
    );
}
```

선택 상자는 버튼 응답에 팝업을 연결한다.

```rust
let response = settings_select_button(ui, current_label);
egui::Popup::menu(&response).show(|ui| {
    ui.set_min_width(SETTINGS_DETAIL.select_width);
    // selectable_label 목록
});
```

## 8. 금지 사항

- 화면마다 15/14/13px, 68px, 230px 같은 값을 다시 선언하지 않는다.
- 기본 `ComboBox`, 기본 checkbox를 그대로 섞어 참조 화면과 다른 형태를 만들지 않는다.
- `▾`, 테마 아이콘 등을 폰트 글리프에 의존하지 않는다.
- 상세 행 구분선에 `hairline_full`을 사용해 26px 여백을 침범하지 않는다.
- `config.ui.ui_font = None`을 닫힌 컨트롤에서 단순 `기본(자동)`으로 표시하지 않는다.
- 관리 화면에 `SettingsDetailShell`을 중첩해 이중 padding을 만들지 않는다.
- 사용자 제공 `design/` 파일을 앱 구현 과정에서 임의로 수정하지 않는다.

## 9. 변경 후 검증

컴포넌트 토큰을 변경하면 `settings.rs`의 치수/타이포 회귀 테스트도 함께 갱신한다.
화면이 마음에 들어 보인다는 이유만으로 테스트 기대값만 느슨하게 만들지 않는다.

```bash
cargo fmt --all -- --check
cargo test -p deppy-sijo ui::settings
cargo test -p deppy-sijo fonts::tests
cargo run -p xtask -- i18n-check
cargo clippy -p deppy-sijo --all-targets -- -D warnings
git diff --check
```

릴리스 앱 반영이 필요하면 다음을 추가로 수행한다.

```bash
scripts/package-macos.sh
open "target/bundle/Deppy Sijo.app"
```

## 10. 리뷰 체크리스트

- [ ] 폰트명과 실제 적용 폰트가 일치한다.
- [ ] 제목 15px, 행 제목 14px, 설명 13px, 컨트롤 14px다.
- [ ] 좌우 26px, 행 68px, 우측 열 360px, gap 20px다.
- [ ] 선택 상자 230×34px, 테마 세그먼트 각 104×34px다.
- [ ] 토글 46×26px, 스텝퍼 226×34px다.
- [ ] 설명색과 muted 색을 혼동하지 않았다.
- [ ] 좁은 창에서 라벨과 컨트롤이 겹치지 않는다.
- [ ] 테마를 바꿔도 터미널 pane은 다크 상태를 유지한다.
- [ ] 한국어 외 모든 locale에 동일 키가 존재한다.
- [ ] 테스트, i18n-check, Clippy, `git diff --check`가 통과한다.
