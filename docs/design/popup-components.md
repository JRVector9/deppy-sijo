# 공용 팝업 디자인과 구현 규칙

기준 시안: [42개 팝업 사례](../mockups/shared-popup-components-2026-09-30.html). 이 문서는 새 팝업을 만들거나 기존 팝업을 옮길 때 사용하는 구현 계약이다. 사례 번호는 시안의 왼쪽 목록과 전체 표에서 같다.

## 적용 범위

| 사례 | 화면 | 상태 | 코드 |
| --- | --- | --- | --- |
| 01 | 워크스페이스 추가 | 공용 팝업 적용 | `crates/app/src/workspace_add.rs` |
| 02 | 새 폴더 만들기 | 공용 팝업 적용 | `crates/app/src/ui/file_tree.rs` |
| 03 | 새 파일 만들기 | 공용 팝업 적용 | `crates/app/src/ui/file_tree.rs` |

AI 세션 시작 런처는 기존 전용 레이아웃을 유지한다. macOS 기본 파일 선택창과 별도 설정 창에도 이 팝업 셸을 씌우지 않는다. 나머지 사례는 실제로 변경할 때 이 문서의 규칙과 시안을 비교해 적용한다. 번호만으로 아직 구현됐다고 간주하지 않는다.

## 컴포넌트 경계

`crates/app/src/ui/popup/` 아래 코드는 표시만 담당한다. 파일 선택, Git 복제, 파일 생성, 오류 상태, 입력 초안은 각 호출자가 계속 소유한다.

| 컴포넌트 | 파일 | 책임 |
| --- | --- | --- |
| `PopupSpec` / `show` / `body` | `shell.rs` | 모달 배경, 폭, 제목·설명·닫기, 고정 머리글과 스크롤 본문 |
| `field` / `segmented_choice` | `fields.rs` | 레이블·힌트·입력 높이, 두 선택지의 같은 폭 |
| `notice` | `notice.rs` | 일반 안내와 오류 안내 |
| `footer` / `action_button` | `actions.rs` | 구분선, 하단 버튼 정렬, 기본·보조 버튼 |
| 팔레트 | `mod.rs` | 다크·라이트 색상 토큰 |

새 창은 화면마다 `egui::Modal`, 제목 여백, 버튼 색을 다시 만들지 않는다. `popup::show`에 고유한 `egui::Id`, 폭, 지역화된 제목·설명·닫기 문구를 전달하고, `popup::body`와 `popup::footer` 안에 화면 고유 입력·동작을 배치한다. `show`가 반환하는 `bool`은 닫기 버튼·바깥 클릭·Esc 요청이다. 비동기 작업 중 닫으면 안 되는 창은 `close_enabled: false`로 전달한다. 사용자 입력과 실제 작업은 반환된 요청을 호출자에서 처리한다.

```rust
let close_requested = popup::show(ctx, popup::PopupSpec {
    id: egui::Id::new("unique_dialog_id"),
    width: 420.0,
    title: &title,
    subtitle: &subtitle,
    close_label: &close_label,
    close_enabled: true,
}, |ui| {
    popup::body(ui, |ui| {
        popup::field(ui, &name_label, Some(&location), |ui| {
            ui.add(egui::TextEdit::singleline(&mut draft)
                .desired_width(ui.available_width()));
        });
    });
    popup::footer(ui, |ui| {
        // right_to_left 레이아웃: 기본 버튼을 먼저 그려 오른쪽에 둔다.
        if popup::action_button(ui, &create_label, popup::ActionTone::Primary, true).clicked() {
            submit = true;
        }
        if popup::action_button(ui, &cancel_label, popup::ActionTone::Secondary, true).clicked() {
            cancel = true;
        }
    });
});
```

## 치수와 시각 규칙

- 긴 양식 폭 560pt(01), 짧은 생성 양식 폭 420pt(02·03). 사용 가능한 화면 폭에서 좌우 16pt를 남기도록 제한한다.
- 제목 18pt, 설명·레이블 12pt, 힌트 11pt, 버튼 13pt. 머리글 좌우 22pt, 위 19pt, 아래 16pt. 본문 좌우 22pt, 위 18pt, 아래 21pt. 하단 좌우 22pt, 위아래 12pt.
- 입력 높이 36pt, 하단 버튼 최소 높이 34pt. 본문만 스크롤하고 머리글·하단 버튼은 유지한다.
- 다크 색상은 시안의 `--surface`, `--line`, `--text`, `--muted`, `--accent`를 따르며 라이트 팔레트도 별도로 제공한다. 안내문은 정보/오류 톤을 구분한다.
- 모든 사용자 표시 문구는 `i18n::Catalog`에서 가져온다. 힌트·오류·상태 문구가 길어져도 잘릴 수 있으므로 짧은 창에 고정 높이를 주지 않는다.

## 상호작용 규칙

- 새 폴더·새 파일은 이름 입력에 바로 초점을 주고, Enter는 만들기, Esc·취소·닫기는 입력을 버린다. 파일 트리 행을 밀지 않도록 모달은 트리 레이아웃 밖에 둔다.
- 워크스페이스 추가는 `내 폴더`/`GitHub 저장소` 선택, URL·저장 위치·폴더 이름의 초안, 복제 상태와 오류를 기존 `WorkspaceAddUi`가 관리한다. 하단의 주 동작은 선택 방식에 맞춰 바뀐다. 복제 중에는 닫기를 막고 복제 취소만 허용한다.
- 시안의 01번은 GitHub 입력 상태를 보여주는 예시다. 실제 첫 선택은 기존과 같이 `내 폴더`이며, 사용자가 GitHub를 누르면 저장소 입력으로 전환한다.
- 모달 UI 안에서 디스크·네트워크 작업을 시작하거나 동기 대기하지 않는다. 호출자가 의도를 받아 기존 작업 경로로 보낸다.
- 레이블·버튼·안내문은 각 언어의 같은 키를 유지한다. 화면을 옮길 때 기존 키를 재사용하거나 모든 지원 언어에 새 키를 동시에 추가한다.

## 다음 팝업을 적용할 때

1. 시안의 사례 번호와 현재 코드 위치를 확인한다. 전용 런처·OS 창인지 먼저 판별한다.
2. 호출자 상태와 실제 명령을 유지한 채 `popup` 컴포넌트에 표시 코드만 연결한다. 폭은 400/420/480/560pt 중 내용에 맞춰 고르고 화면 폭 제한은 셸에 맡긴다.
3. 닫기·확인·취소·Enter·작업 중 비활성화 규칙을 적고, 실제 동작을 검증하는 UI 테스트를 먼저 실패시킨 뒤 변경한다.
4. 해당 사례의 HTML 코드 위치와 상태, 이 문서의 적용 범위를 갱신한다. 집중 테스트, i18n 테스트, 형식·Clippy·빌드를 실행한 결과를 `docs/CODEX_HANDOFF.md`에 기록한다.

Deppy를 실행하거나 재실행하는 것은 별도 명시적 요청이 있을 때만 한다.
