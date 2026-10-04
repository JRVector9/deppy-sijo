# 공용 팝업 디자인과 구현 규칙

기준 시안: [42개 팝업 사례](../mockups/shared-popup-components-2026-09-30.html). 이 문서는 새 팝업을 만들거나 기존 팝업을 옮길 때 사용하는 구현 계약이다. 사례 번호는 시안의 왼쪽 목록과 전체 표에서 같다.

## 적용 범위

| 사례 | 화면 | 상태 | 코드 |
| --- | --- | --- | --- |
| 01 | 워크스페이스 추가 | 공용 팝업 적용 | `crates/app/src/workspace_add.rs` |
| 02 | 새 폴더 만들기 | 공용 팝업 적용 | `crates/app/src/ui/file_tree.rs` |
| 03 | 새 파일 만들기 | 공용 팝업 적용 | `crates/app/src/ui/file_tree.rs` |
| 04 | 워크스페이스 이름 바꾸기 | 워크스페이스 행에서 인라인 편집 | `crates/app/src/ui/file_tree.rs` |
| 06 | MCP 서버 추가·수정 | 공용 중앙 이동·크기 조절 입력 창 | `crates/connector-ui/src/lib.rs` |
| 07 | 실행 중인 세션 닫기 | 공용 확인 팝업 · 대상별 입력 보호 | `crates/app/src/ui/session_close_dialogs.rs` · 호출자 `ui/workspace.rs` |
| 08 | 워크스페이스 세션 모두 종료 | 공용 확인 팝업 · 대상별 입력 보호 | `crates/app/src/ui/session_close_dialogs.rs` · 호출자 `app.rs` |
| 09 | 환경 및 API 프로젝트 목록 닫기 | 공용 확인 팝업 · 세션 유지·대상별 입력 보호 | `crates/app/src/ui/environment_dialogs.rs` · 호출자 `app.rs` |
| 10·11 | 리소스 세션·미연결 프로세스 종료 | 공용 확인 팝업 적용 | `crates/app/src/ui/resource_manager.rs` |
| 12 | 포트 프로세스 종료 | 공용 확인 팝업 적용 | `crates/app/src/ui/ports.rs` |
| 13 | 휴지통 실패 후 영구 삭제 | 공용 확인 팝업 적용 | `crates/app/src/ui/file_tree.rs` |
| 15 | 환경변수 정의 삭제 | 공용 팝업 · 36pt 원본 파일 선택·명시적 삭제 | `crates/app/src/ui/environment_dialogs.rs` · 호출자 `ui/env_profiles.rs` |
| 18–20 | 다음 단계 예약·브로드캐스트·일괄 시작 | 공용 중앙 이동·크기 조절 창·고정 하단 버튼 | `crates/app/src/ui/fleet.rs` |
| 22 | MCP 도구 직접 호출 | 공용 중앙 이동·크기 조절 입력 창 | `crates/connector-ui/src/lib.rs` |
| 26 | 미저장 문서 닫기 | 공용 팝업 · 저장/버리기/취소 | `crates/app/src/ui/document_dialogs.rs` |
| 27 | 외부 수정 문서 다시 불러오기 | 공용 팝업 · 다시 불러오기/취소 | `crates/app/src/ui/document_dialogs.rs` |
| 28 | 문서 탭 한도 | 공용 Info 팝업 | `crates/app/src/ui/document_dialogs.rs` |
| 29 | 프로젝트 폴더 이동 | 공용 팝업 · 이전/현재 경로·접수/실패 상태 | `crates/app/src/ui/document_dialogs.rs` |
| 30–32 | 프롬프트 라이브러리·변경사항·세션 관리 | 공용 중앙 이동·크기 조절 셸과 본문 여백 | `ui/prompt_palette.rs` · `ui/diff_panel.rs` · `ui/agent_sessions.rs` |
| 33–35 | 이벤트 적체·워크스페이스 한도·셀 열기 실패 | 공용 Info 팝업 · 순차 안내 | `crates/app/src/ui/popup/information.rs` · 호출자 `app.rs` |
| 37 포트 | 포트 관리 목록 | 공용 앵커 팝오버 · 목록 행·스크롤·고정 푸터 | `crates/app/src/ui/ports.rs` · 호출자 `ui/agent_terminal.rs` |

AI 세션 시작 런처는 기존 전용 레이아웃을 유지한다. macOS 기본 파일 선택창과 별도 설정 창에도 이 팝업 셸을 씌우지 않는다. 나머지 사례는 실제로 변경할 때 이 문서의 규칙과 시안을 비교해 적용한다. 번호만으로 아직 구현됐다고 간주하지 않는다.

## 컴포넌트 경계

`crates/connector-ui/src/popup/`가 공용 표시 구현을 소유하고 `crates/app/src/ui/popup/`가 기존 경로에서 이를 재공개한다. App 전용 확인·정보 안내·터미널 입력 차단은 App에 남는다. 두 호출자는 같은 팔레트와 모달 입력 펜스를 사용한다. 파일 선택, Git 복제, 파일 생성, 오류 상태, 입력 초안은 각 호출자가 계속 소유한다.

| 컴포넌트 | 파일 | 책임 |
| --- | --- | --- |
| `WindowSpec` / `window` / `window_body` | `connector-ui/src/popup/window.rs` | 중앙에서 열리는 이동·크기 조절 창, 본문 스크롤과 고정 하단 여백 |
| `PopupSpec` / `show` / `body` | `shell.rs` | 모달 배경, 폭, 제목·설명·닫기, 고정 머리글과 스크롤 본문 |
| `field` / `text_input` / `path_input` / `segmented_choice` | `fields.rs` | 레이블·힌트, 36pt 입력, 경로와 찾아보기 행, 두 선택지 |
| `choice_input` | `fields.rs` | 36pt 선택 입력, 공용 테두리·폰트·배경 |
| `popover` / `popover_frame` / `body_with_max_height` | `shell.rs` | 모달과 같은 머리글·팔레트, 앵커 팝오버와 높이 제한 |
| `information` / `InformationSpec` | `information.rs` | 420pt 정보 안내·공용 notice와 닫기 동작 |
| `list_row` / `list_actions` | `list.rs` | 목록 표면·줄바꿈 행·34pt 버튼 배치 |
| `notice` | `notice.rs` | 일반 안내와 오류 안내 |
| `footer` / `action_button` | `actions.rs` | 1pt 구분선, 단축키 안내, 8pt 간격·줄바꿈, 네 종류 버튼·실측 하단 높이 |
| `confirmation` / `ConfirmationSpec` | `confirmation.rs` | 대상 경로·이름, 오류 톤 경고, Danger 확인·Ghost 취소, 닫기 판단 |
| `prepare_target` / `confirmation_for_target` | `shell.rs` / `confirmation.rs` | 케이스별 고정 Area·하단 캐시, 대상별 동작 ID·포커스 해제 |
| 입력 소유권 | `input.rs` | viewport/pass별 예정·표시 모달 차단, 검색·메뉴와 구분 |
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
            popup::text_input(ui, &mut draft, "");
        });
    });
    popup::footer(ui, Some(&keyboard_hint), |ui| {
        // right_to_left 레이아웃: 기본 버튼을 먼저 그려 오른쪽에 둔다.
        if popup::action_button(ui, &create_label, popup::ActionTone::Primary, true).clicked() {
            submit = true;
        }
        if popup::action_button(ui, &cancel_label, popup::ActionTone::Ghost, true).clicked() {
            cancel = true;
        }
    });
});
```

## 치수와 시각 규칙

- 긴 양식 폭 560pt(01), 짧은 생성 양식 폭 420pt(02·03). 사용 가능한 화면 폭에서 좌우 16pt를 남기도록 제한한다.
- 제목 18pt, 설명·레이블 12pt, 힌트 11pt, 버튼 13pt. 머리글 좌우 22pt, 위 19pt, 아래 16pt. 본문 좌우 22pt, 위 18pt, 아래 21pt. 하단 좌우 22pt, 위아래 12pt.
- 입력 바깥 높이 36pt, 안쪽 세로 8pt·가로 10pt. `text_input`이 `add_sized`와 자체 margin을 함께 설정한다. 부모 `Ui`의 `interact_size`만 바꾸거나 호출자가 기본 `TextEdit`를 직접 그리지 않는다.
- 버튼 높이 34pt, 좌우 여백 13pt·세로 여백 6pt, 버튼 사이 8pt. 기본 버튼은 Primary, 찾아보기·복제 취소는 Secondary, 창 취소는 Ghost를 사용한다. 너비는 글자 폭과 여백으로 계산하므로 긴 번역도 같은 규칙을 따른다.
- 저장 위치 행은 `path_input`으로 표시한다. 입력 36pt와 찾아보기 34pt를 중앙 정렬하고 7pt 간격을 둔다. 버튼의 실제 글자 폭을 뺀 나머지가 입력 너비다. 같은 행에 들어가지 않는 좁은 공간에서는 아래 줄로 바꾼다.
- 레이블 아래 7pt, 입력 힌트 위 6pt, 필드 사이 16pt, 방식 선택 아래 17pt. 제목 줄 높이 23.4pt, 설명 줄 높이 18pt. 닫기 버튼은 제목 줄의 높이를 늘리지 않는 별도 30pt 대상이다. 구분선은 1pt만 차지한다.
- 방식 선택은 청록색으로 채우지 않고 회색 raised 배경·테두리로 표시한다. 안내 박스는 본문 전체 폭을 쓰고 정보/오류 별도 테두리와 글자 색을 사용한다. 버튼과 탭은 hover·키보드 focus·비활성 상태를 제공한다.
- 본문만 스크롤하고 머리글·하단 버튼은 유지한다. 좁은 화면에서는 버튼 전체를 다음 줄로 넘기며 행 사이도8pt다. 단일 버튼 문구가 전체 본문 폭보다 길 때만 문구를 줄바꿈한다. 짧은 버튼은34pt를 유지한다. 보조 단축키 안내는 주 동작 버튼을 밀어내지 않는다.
- 버튼은 개별 child scope 대신 부모의 wrapping 레이아웃에 직접 배치한다. 남은 행 폭으로 문구를 다시 측정하지 않고 전체 행 폭으로 galley를 만든다. footer의 실측 높이를 케이스별 고정 키에 저장하여 다음 pass 본문 스크롤 높이에 반영한다. 높이/크기 변화 때만 다시 그리며 문서별 키를 누적하지 않는다.
- 본문 높이는 실제 머리글 높이와 하단 버튼·여백을 제외한 화면 공간으로 계산한다. 방식 전환으로 본문 길이가 달라져도 이전 모달 크기로 스크롤 영역을 제한하지 않으며, 크기 변경 후 한 번 더 그려 중앙 위치와 클릭 위치를 맞춘다.
- 다크 색상은 시안의 `--surface`, `--line`, `--text`, `--muted`, `--accent`를 따르며 라이트 팔레트도 별도로 제공한다. 안내문은 정보/오류 톤을 구분한다.
- 모든 사용자 표시 문구는 `i18n::Catalog`에서 가져온다. 힌트·오류·상태 문구가 길어져도 잘릴 수 있으므로 짧은 창에 고정 높이를 주지 않는다.

## 상호작용 규칙

- 새 폴더·새 파일은 이름 입력에 바로 초점을 주고, Enter는 만들기, Esc·취소·닫기는 입력을 버린다. 파일 트리 행을 밀지 않도록 모달은 트리 레이아웃 밖에 둔다.
- 워크스페이스 추가는 `내 폴더`/`GitHub 저장소` 선택, URL·저장 위치·폴더 이름의 초안, 복제 상태와 오류를 기존 `WorkspaceAddUi`가 관리한다. 하단의 주 동작은 선택 방식에 맞춰 바뀐다. 복제 중에는 닫기를 막고 복제 취소만 허용한다.
- 로컬 폴더 등록은 열린 디렉터리 descriptor에서 dev/inode와 macOS 볼륨 UUID를 같은 객체 기준으로 읽어 settings worker에서 저장한다. 기존 UUID와 inode가 일치할 때만 재마운트의 장치 번호 변경을 허용한다. 다른 볼륨·식별 정보가 없는 장치 변경은 기존 workspace를 자동 재사용하지 않고 오류 안내를 표시한다. 설정의 명시적 프로젝트 경로 재연결은 같은 경로/dev/inode여도 이전 증명을 트랜잭션 안에서 교체한다.
- 시안의 01번은 GitHub 입력 상태를 보여주는 예시다. 실제 첫 선택은 기존과 같이 `내 폴더`이며, 사용자가 GitHub를 누르면 저장소 입력으로 전환한다.
- URL 아래에는 지원 형식 안내를 표시한다. 하단 단축키는 생성 창에서 `Enter 만들기 · Esc 닫기`, 워크스페이스 추가에서 `Esc 닫기`다. 워크스페이스 복제는 버튼으로 시작하므로 Enter 실행이라고 표기하지 않는다.
- 모달 UI 안에서 디스크·네트워크 작업을 시작하거나 동기 대기하지 않는다. 호출자가 의도를 받아 기존 작업 경로로 보낸다.
- 종료·영구 삭제는 `confirmation`에 ID·제목·설명·대상(선택)·경고·확인/취소/닫기 레이블을 전달한다. `Confirm`/`Cancel`만 반환하며, 작업은 호출자가 기존 경로에서 접수한다. 대상은 세션·runtime instance·소켓 시작 식별자·삭제 대상의 원래 값을 보존한다. 영구 삭제는 큐가 실제 접수한 뒤에만 확인을 닫으며, 접수 거절 사유도 확인창 안에 표시한다.
- 위험 동작 버튼은 Danger, 취소는 Ghost다. 기본 Enter만으로 위험 동작을 실행하지 않는다. 취소·X·바깥 클릭·Esc는 닫기이며, X의 접근성 이름은 `popup.dismiss`로 취소 버튼과 구분한다. 앞쪽 모달이 있으면 뒤 터미널의 검색·일반 입력·IME·파일 단축키를 차단한다.
- 07·08의 지역화된 표시 사양은 `ui/session_close_dialogs.rs`의 `session`·`workspace`를 재사용한다. 확인 대상은 표시 이름 대신 원래 pane/workspace ID로 구분한다. `confirmation_for_target`은 제목이 같아도 대상 변경 시 이전 버튼 포커스를 해제하고 action ID를 분리한다. `prepare_target`은 문서 팝업26·27에도 같은 구현을 사용한다. Modal Area·하단 높이 캐시는 케이스별로 유지하고, 대상별로 한도 없이 쌓지 않는다. 세션 종료 명령·워크스페이스 종료 의도는 각각 WorkspaceUi/App이 기존 경로로 접수한다. 워크스페이스의 확인 ON/OFF 설정 및 종료된 세션의 즉시 정리 정책은 유지한다.
- 리소스·포트 확인은 원래 팝오버 밖에서 그린다. 원래 메뉴가 닫혀도 확인 대상은 유지하며, Esc는 가장 앞 모달을 취소한다.
- 전역 세션·워크스페이스 단축키도 모달·작성 양식·메뉴가 열려 있으면 배경에 전달하지 않는다. 예정 모달을 전역 단축키·터미널보다 먼저 공개하고, 공용 Modal/런처/자격증명 창도 표시 시 viewport/pass별 fence를 등록한다. 닫힌 pass 끝까지 차단하며 다음 pass에 만료된다. 실제 모달·열린 메뉴·기존 Middle 작성창만 차단하고 일반 Foreground 검색창·툴팁·닫힌 상황 메뉴는 차단 근거로 쓰지 않는다. 조회용 에이전트·diff 창은 기존 비모달 동작을 유지한다.
- 기존 Window형 예약·브로드캐스트·일괄 시작·프롬프트 창은 `popup::take_window_escape`를 재사용한다. Esc는 맨 앞 창 한 곳에서만 소비하며, 앞쪽 모달·선택 메뉴가 있으면 뒤의 초안을 취소하지 않는다.
- 새 파일·폴더의 검증·작업 실패는 생성 폼 안의 Error notice에 표시한다. 실패한 이름·생성 위치를 유지해 고쳐 다시 시도할 수 있게 한다.
- MCP 승인·원격 신뢰·OAuth 창이 있으면 로컬 서버 작성·도구 실행·삭제 양식을 잠시 표시하지 않고 초안을 유지한다. 서비스 요청을 해결하면 기존 양식으로 돌아온다.
- 01번의 로컬 폴더 열기·Git 복제가 런타임 워크스페이스로 등록·전환되면 터미널 화면을 드러내고 전용 런처를 연다. 새 폴더·재선택·이미 활성인 폴더 모두 적용하며, 런처에서 선택하기 전 기본 셸을 자동 생성하지 않는다. 설정의 프로젝트 등록·저장 위치 선택은 해당 설정 작업을 수행한다. 전환 실패와 진행 중인 런처 실행 요청은 그대로 보존한다.
- 워크스페이스 이름은 새 팝업을 열지 않고 해당 행에 커서를 둔다. 기존 세션 이름 편집 도우미를 재사용하고 처음 한 번만 포커스를 받는다. Enter 저장, Esc·행 밖 클릭·다른 화면 전환 취소이며, 실제 폴더·경로는 바꾸지 않는다. 빈 별칭은 폴더 이름으로 돌아간다. 완료/취소/대상 소멸 시 입력 상태를 해제한다.
- 인라인 이름 편집의 Enter·Esc 처리 프레임은 포커스를 해제한 후에도 배경 입력을 막는다. 편집창이 소유한 egui 이벤트와 별도 native key batch를 함께 소비해, 붙여넣기·문장부호가 터미널로 다시 전달되지 않게 한다. 차단은 다음 pass에 해제된다.
- 휴지통 실패로 생긴 FileTree 영구 삭제 확인은 실제 표시 전에도 대기 모달이다. App은 worker 결과 처리 후 전역 단축키보다 먼저 이 상태를 입력 차단에 포함한다. 확인창은 파일 목록의 가시성과 분리해 패널을 접거나 메모 탭을 선택해도 표시·취소할 수 있게 한다.
- MCP 승인창은 고정 Window Area를 유지하며 요청이 바뀔 때 두 고정 버튼 ID 묶음을 번갈아 사용한다. egui의 포커스 캐시에 요청마다 새로운 버튼 ID를 누적하지 않는다. 대상이 바뀌면 이전 포커스를 해제하고, 사용자가 새로 버튼을 선택한 뒤에만 Enter로 결정한다.
- 레이블·버튼·안내문은 각 언어의 같은 키를 유지한다. 화면을 옮길 때 기존 키를 재사용하거나 모든 지원 언어에 새 키를 동시에 추가한다.

## 문서·경로 팝업 26–29

`ui/document_dialogs.rs`는 지역화된 내용을 조합하고 선택 의도만 반환한다. 공용 `popup::show/body/footer/notice/action_button`을 사용하며 파일 I/O나 runtime 명령을 실행하지 않는다. App이 문서 큐 앞 항목의 ID, 저장 자격과 저장 후 닫기 continuation을 소유한다.

- 26은480pt, 파일명과 버리기 경고·대기 문서 수·저장 상한 이유를 표시한다. Save는 `can_save_then_close` 자격을 그대로 전달하고 Secondary, Discard는 Danger, Cancel은 Ghost다. 저장 중 continuation 자격을 일반 `can_save`로 바꾸지 않는다.
- 27은400pt, Reload는 Danger다. 미저장 내용이 사라진다는 경고와 대기 수를 유지한다. 28은420pt, Info notice와 Primary 닫기만 제공한다. 26·27·28의 X·바깥 클릭·Esc는 취소/안내 닫기이고, 기본 Enter는 위험 동작을 실행하지 않는다.
- 26·27은 문서 탭 ID를 Modal 케이스 ID와 별도로 전달한다. Modal Area는 케이스별 고정 ID를 유지하고, 버튼은 대상 ID로 구분한다. 대상 변경 시 이전 focus를 해제하여 같은 파일명인 다음 문서에도 위험 동작을 승계하지 않는다.
- 비동기 문서 결과는 전역 단축키보다 먼저 수신하고, 사이드바/상단 버튼이 확인창을 열면 검색·컴포저·주/부착 터미널보다 먼저 fence를 갱신한다. 예약된 세션 닫기도 예정 모달에 포함하며 입력 활성 여부와 독립적으로 먼저 표시한다.
- 문서 확인 → 한도 안내 → 경로 안내 순서로 하나만 표시한다. 큐 앞 항목을 처리한 프레임에 다른 경로 확인을 함께 올리지 않는다.
- 29는480pt, 이전/현재 경로를 각각 줄바꿈 가능한 행으로 표시한다. 감지 시 workspace ID·옛 경로·새 경로·inode anchor를 캡처한다. 다른 settings 작업이 대기 중이면 Update를 비활성화하고 안내를 유지한다. 실제 접수한 operation의 generation/revision/workspace ID를 보존하며 중복 갱신과 닫기를 막는다. 일치하는 결과만 이 안내를 정리한다. 실패는 원래 경로와 오류 notice를 유지하고 재시도 가능하게 한다. 아직 worker에 접수되지 않은 작업을 settings navigation이 폐기하면 안내를 다시 활성화한다. 저장 worker의 CAS는 유지한다.
- `popup::take_modal_escape`가 공용 확인과 문서 팝업의 Esc 소유권을 함께 검사한다. 앞 모달만 키를 소비하며 뒤 메뉴가 모달 취소를 막지 않는다.

탭바: 세션 X 오른쪽에 픽셀 스냅 구분선을 항상 그린다. 포커스된 헤더의 세로선은 상단선과 같은 색·두께를 쓰며, 같은 픽셀 좌표에서 끊김 없이 이어져 헤더 하단까지 내려간다. 기존 세션/문서 탭은 선택·닫기를 유지하고, 모든 보이는 탭 뒤부터 도구 버튼 앞까지 빈 영역은 기존 AI 세션 런처를 요청한다. 셸/AI를 자동 생성하지 않는다. 비활성 입력 surface의 첫 클릭은 초점 요청만 한다.

## 환경 설정·안내·포트 목록 09·15·33–35·37

`ui/environment_dialogs.rs`는 프로젝트 목록 닫기와 변수 삭제 표시만 담당한다.09는400pt, Primary 닫기·Ghost 취소를 쓰며 실행 중인 세션은 유지한다.15는480pt, 변수 이름·Error notice·`choice_input` 원본 선택·Danger 삭제·Ghost 취소를 쓴다. 원본 파일 목록과 선택 초안은 `EnvProfilesUi`가 소유하고, 기존 `DotenvWrite`는 호출자가 처리한다. 기본 Enter는 삭제하지 않는다. X·Esc·바깥 클릭은 취소하고 선택 초안을 정리한다. 사례별 고정 Area와 최신 대상+두 개 action scope를 사용해 새 프로젝트/변수에 이전 버튼 포커스를 승계하거나 대상 수만큼 action ID를 누적하지 않는다.

원본 선택 메뉴가 열려 있으면 첫 Esc는 그 메뉴만 닫고 선택한 파일과 확인창을 보존한다. 메뉴가 닫힌 뒤 다음 Esc는 확인창을 취소한다. 이 입력 처리는 `choice_input`에서 하위 메뉴가 실제로 그려진 pass에만 수행한다.

33–35는 공용 `information`으로 제목·Info notice·Primary 닫기를 표시한다. App은 대기 상태를 배경 단축키보다 먼저 fence에 포함하고, 같은 pass에 세 안내를 겹쳐 표시하지 않는다. 확인·문서·이동 등 기존 모달의 대상 상태는 유지한다.

37의 포트 부분은 상태바 앵커 팝오버다. 배경 모달을 만들지 않고 공용 머리글·표면·닫기·본문·고정 푸터를 재사용한다. 폭은560pt 이하로 제한한다. 본문은 viewport와560pt 상한을 함께 적용한 한 개 ScrollArea이고 현재/다른/외부 섹션과 `list_row`를 그린다. 행 동작은 `list_actions`가 실제34pt 높이로 배치하며, 스크롤 영역의 남은 높이를 버튼 행 높이로 사용하지 않는다. 새로고침은 푸터에서 기존 의도만 보내며 새 폴링을 추가하지 않는다. 주소 복사는 IPv4/IPv6/와일드카드 소켓 문자열을 그대로 유지한다. 외부·보호·소유권 불명 프로세스는 읽기 전용이다. 행의 종료는 기존12번 확인창을 열 뿐이고 확인창은 원래 팝오버 외부에서 원래 소유권/프로세스 시작 식별자를 유지한다. 승인·리소스 팝오버의 기존 표시 구조는 이번 변경에 포함하지 않는다.

HTML의37번은 상세 포트 목록을 보여준다. 번호는 기존42개를 유지하며 `#terminal_status`, `#env_project_close`, `#env_delete`, `#overflow`, `#warm_limit`, `#cross_pane_failure`로 해당 화면을 직접 열 수 있다. 시안의 포트 종료를 누르면 선택한 소켓/워크스페이스로12번을 보여주고 취소 시 목록으로 돌아간다.

## 다음 팝업을 적용할 때

1. 시안의 사례 번호와 현재 코드 위치를 확인한다. 전용 런처·OS 창인지 먼저 판별한다.
2. 호출자 상태와 실제 명령을 유지한 채 `popup` 컴포넌트에 표시 코드만 연결한다. 폭은 400/420/480/560pt 중 내용에 맞춰 고르고 화면 폭 제한은 셸에 맡긴다.
3. 닫기·확인·취소·Enter·작업 중 비활성화 규칙을 적고, 실제 동작을 검증하는 UI 테스트를 먼저 실패시킨 뒤 변경한다.
4. 해당 사례의 HTML 코드 위치와 상태, 이 문서의 적용 범위를 갱신한다. 집중 테스트, i18n 테스트, 형식·Clippy·빌드를 실행한 결과를 `docs/CODEX_HANDOFF.md`에 기록한다.

Deppy를 실행하거나 재실행하는 것은 별도 명시적 요청이 있을 때만 한다.

## 동작 점검 기록

[2026-09-30 전체 팝업 점검](../reviews/2026-09-30-popup-behavior-audit.md)은 42개 사례의 코드 경로·기존 테스트와 이번 회귀 검증 범위를 구분한다. `cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo popup_audit -- --test-threads=1`과 `cargo test --offline --locked -q -p connector-ui popup_audit -- --test-threads=1`은 앱을 띄우지 않고 재열기 런처·입력 소유권·초안 보존·폼 안 오류 표시를 검증한다.

## 시각 검증 하네스

`cargo test --locked -q -p deppy-sijo --bin deppy-sijo popup_parity -- --test-threads=1`은 실제 WorkspaceAddUi의 입력 높이, 취소 버튼 여백/간격, 경로 행 정렬을 검증한다.

`cargo test --locked -q -p deppy-sijo --bin deppy-sijo popup_parity_render -- --ignored --test-threads=1`은 화면 밖에서 `target/popup-parity/`에 렌더링한다. 01·02·03 생성 양식과 04 실제 인라인 행, 07 세션, 10·11 리소스, 12 포트, 13 영구 삭제는 실제 호출자를 사용한다. 08은 App이 호출하는 실제 `session_close_dialogs::workspace`를 렌더링하며 전체 App 동작 테스트는 아니다. `close_popup`은 같은 이름의 다른 세션·워크스페이스로 대상이 바뀔 때 Enter만으로 새 대상을 종료하지 않는지와 명시 확인을 검사한다. Deppy 앱 프로세스를 실행하지 않는다. GPU는 이 ignored 렌더 테스트를 직접 호출했을 때만 초기화된다.


26–29 검증: `cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo document_popups -- --test-threads=1`. `popup_parity_render_document_popups -- --ignored --test-threads=1`은 실제 표시 함수의 PNG26–29를 생성한다. App 전체를 클릭하거나 실제 파일을 버리는 테스트는 아니다. `tab_strip`은 빈 클릭/비활성 초점/세션 없는 헤더/실제 세로선 paint를 검증한다.

5건 회귀 수정 검증: `popup_review`는 연속 문서 focus, 첫 프레임 PTY/전역 키, 실제 검색·인라인 편집,5개 언어의280×360pt footer와 native UUID/worker 경로를 확인한다. Storage의 같은 필터는 미확인·다른 볼륨, 재마운트, 명시적 재연결, 무효화와 rollback을 검사한다. 전체 실행 결과는 최신 handoff/리뷰 보고서를 따른다.

## Fleet 입력 상한 보정 (2026-10-04)

사례18·19·20의 기존 Window 표시/맨 앞 창 Esc 계약을 유지한다. 다음 단계 예약은 일반 폰트4행 입력이며 UTF-8 본문16KiB, undo8개와 열 때 한 번만 고정 입력 ID를 초기화한다. 입력/템플릿 삽입 상한 거부와 호스트 예약 거부는 공용 Error notice로 안내하며 기존 폼을 보존한다. 호스트가 정확한 원래 대상의 예약을 받아들인 뒤에만 폼을 닫는다. 배치 시작은 확장 결과16KiB를 실제 Start 전에 확인하고, 브로드캐스트의1MiB 결과 상한은 유지한다.

## 중앙에서 열리는 이동·크기 조절 창 (2026-10-05)

[승인된 HTML 시안](../mockups/fleet-centered-resizable-popups-2026-10-05.html)을18–20,06·22,30–32에 적용한다. 기본 다음 단계620×560pt, 브로드캐스트680×560pt, 일괄 시작520×440pt. 각 창의 고유 ID와 native Resize 상태로 크기를 유지하며, 열 때 native CENTER_CENTER pivot을 화면 중앙에 둔다. 이동한 위치는 열린 동안 유지하고 재열기 시 중앙으로 돌아온다. 측정 크기를 다시 기본 크기로 넣는 반복 보정이나 영구 repaint 타이머를 만들지 않는다.

공통 창 셸은18pt 제목·13pt 본문·22pt 좌우 여백·36pt 선택 입력·34pt 동작을 사용한다. 입력 창18–20·06·22는 본문만 스크롤하고 하단 버튼을 고정한다. 라이브러리·변경사항·세션 관리의 내부 목록/분할/동작은 각 호출자가 유지한다. 기존 런처·OS 선택창·앵커 메뉴와 관련 없는 승인/확인창은 이번 이동형 창 변경에 포함하지 않는다. 입력 소유권과 Esc는 기존 호출자 정책을 유지한다.

다음 단계 예약은 실제 모델명 옆에 추론 강도를 표시한다. 기본값은 현재 설정 유지이며, 기존 모델 카탈로그와 검증된 CLI 경로가 모두 있는 강도만 선택 가능하다. 선택한 값은 현재 턴이 끝난 후 원래 세션/실행에 적용한다. PTY 입력 접수는 설정 확인이 아니므로, 설정 접수 이후 Codex는 원래 런타임의 일회성 현재 화면에서 선택값을 확인하고, Claude는 해당 입력 접수 이후의 새 출력에서 CLI 확인 메시지를 확인해야 예약 지시를 보낸다. Claude 확인 버퍼는8KiB이며 worker는 약한 참조만 유지한다. 취소·교체·종료 시 버퍼를 비우고 캡처를 중단한다. 새 완료 세대는 예약 자체에 보존해 완료 알림을 읽어도 이어지는 설정 처리가 멈추지 않으며, 확인 기한은 대기/알림 표시 여부와 독립적으로 검사한다.20초 내 확인하지 못하거나 거절/Unknown이면 예약을 남겨 두고 차단하며 알린다. 취소/교체/종료는 원래 입력 permit을 폐기한다.

Codex는 모델별 단축키 한 단계를 보내고 실제 변경을 확인한 다음 이어서 조정한다. Claude는 `/effort`를 사용한다. 변경 확인을 검증하지 못한 Kimi는 읽기 전용으로 표시한다. Codex Ultra는 별도 대화형 선택이 필요해 자동 변경 목록에 넣지 않으며, Grok의 실행 중 변경 경로는 검증되지 않아 비활성화한다. 지원하지 않는 설정을 프롬프트로 보내거나 새 AI 프로세스를 시작하지 않는다. Claude의 CLI는 선택한 추론 강도를 새 세션 기본값으로도 저장할 수 있다.

## 예약 창 배치·높이 보정 (2026-10-05, 0.7.1)

사례18은 첫 줄 좌측의 `대상` 레이블과 세션명을8pt 간격으로 구분하고 세션명에는 앱의 강조색을 쓴다. 첫 줄 우측에는 실제 모델명과 예약 추론 강도를 함께 표시하는 공용36pt 드롭다운을 둔다. 본문 폭480pt 미만에서는 대상과 드롭다운을 두 줄로 나눠 잘림을 피한다. 모델 자체를 변경하는 메뉴가 아니라 해당 모델의 예약 작업 추론 강도를 고르는 메뉴다. 지원 강도·현재 설정 유지·원래 대상 예약·확인 후 전송 규칙은 동일하다.

프롬프트는 빈 본문/한 줄 본문에서도 최소3줄을 표시하고 창 크기에 맞춰 늘어난다. egui0.35 TextEdit는 min_size.y를 적용하지 않으므로, 실제13pt 폰트 행 높이와16pt 내부 여백을 사용해 desired_rows를 계산한다. 최소 높이136pt와 기존16KiB/undo8 입력 보호를 유지한다. 실제 위젯 높이·선택·텍스트 색과 간격을 offscreen UI로 검증한다.

세션·문서 탭 헤더는29→27pt로 줄인다. 전체 앱 제목바38→36pt는 이전 커밋에 반영돼 있으며 이번 요청의 세션 탭 헤더와 별도다.
