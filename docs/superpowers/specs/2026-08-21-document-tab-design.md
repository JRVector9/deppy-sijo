# 문서 탭 설계 (md·txt 열기/편집/저장)

상태: 2026-08-21 설계, 구현 전
선행 문서: `docs/document-editor-lightweight-core-plan.md`(2026-07-27)
장기 계획: `docs/document-editor-development-plan.md`

## 0. 이 문서가 갱신하는 것

경량 계획의 **방향과 계약은 그대로 유효**하다. `egui_commonmark 0.24`가 egui
0.35와 지금도 빌드되는 것, 파일 트리에 문서 내용 lane이 없다는 것, I/O 계약
(atomic replace·revision 비교·외부 변경 자동 덮어쓰기 금지·source/path 로깅 금지)은
2026-08-21에 재확인했다.

바뀌는 것은 **§4 최소 아키텍처 한 절**이다. 경량 계획은 문서 영역을 중앙
**outer split**에 놓겠다고 했는데, 그 문서가 쓰인 뒤 저장소가 중앙 영역에 다른
기구를 확립했다 — App이 소유하는 **보조 pane 탭**(`PaneAuxTab`)이고 이력(2026-08-14)과
Git(2026-08-15)이 쓴다. 문서도 그 기구를 쓴다.

## 1. 핵심 결정: 보조 탭 = pane 전체

> "탭으로 여는 것"과 "pane 전체로 여는 것"은 **이미 같은 동작**이다.

보조 탭이 활성화되면 `WorkspaceUi::render_pane`이 pane 본문 rect를 App에 돌려주고
터미널 표면·입력·DnD·컨텍스트 메뉴 렌더를 **전부 건너뛴다**. 즉 이력 탭은 지금도
pane 본문 전체를 차지한다. 문서도 같은 자리를 쓴다 — 새 기구를 만들지 않는다.

그래서 outer split을 채택하지 않는다:

- 중앙 영역에 성격이 다른 배치 기구가 둘 생기지 않는다.
- 헤더 탭 전환, 본문 rect 계산, 좌우 분할 폭 기억, 터미널 입력 fail-closed,
  세션 탭 클릭 복귀 — 이력·Git이 이미 갖춘 것을 그대로 쓴다.
- pane 단위라 split한 pane마다 다른 문서를 열 수 있다.

## 2. 탭 상한

`PANE_AUX_TAB_MAX = 2`이고 History·Git이 **정확히 다 차지**하고 있다. 문서 탭을
더하려면 상한을 **3으로 올린다**.

헤더는 세션 제목이 우선이라는 원칙은 유지한다. 좁아질 때의 축약 순서를 못박는다:

1. 각 보조 탭의 `×`를 순서대로 뺀다(문서 → Git → 이력).
2. 그래도 모자라면 탭 자체를 뒤에서부터 뺀다.
3. 세션 제목은 마지막까지 남긴다.

문서 탭이 먼저 축약되는 이유: 문서는 파일명이 라벨이라 길고, 닫기는 툴바에도 있다.

## 3. 대상 파일과 열기 동작

### 3.1 대상

- **Markdown**: `.md`, `.markdown` — source / preview / split
- **평문**: `.txt`, `.log`, 확장자 없는 UTF-8 텍스트 — source 전용(토글 숨김)

그 외 확장자는 **지금 동작을 그대로 유지**한다(`FileTreeIoRequest::OpenPath`로 OS
기본 앱). 문서 대상이어도 OS로 여는 길은 컨텍스트 메뉴에 남긴다 — 기존 동작을
빼앗지 않는다.

### 3.2 트리거

파일 트리에서 문서 파일을 열면 `SidebarAction::OpenDocument(DocumentTarget)`를
올린다. App이 받아 문서 탭을 열고 활성화한다.

**열기 대상 pane**: 포커스된 pane. pane이 하나도 없으면 문서 탭도 붙일 곳이 없으므로
셸 pane을 먼저 만들고 그 위에 연다(이력 탭이 세션 없는 워크스페이스에서 취하는 방식과
같다).

### 3.3 한 pane에 문서 하나

pane당 문서 탭은 하나다. 다른 문서를 열면 그 자리를 **교체**한다. 교체 전 dirty면
확인을 받는다(저장 / 버리기 / 취소).

여러 문서를 동시에 보려면 pane을 split한다 — 터미널과 같은 모델이라 새 개념이 없다.

## 4. 상태 소유

```rust
/// App 소유. leaf UI는 intent만 올린다.
struct DocumentTabState {
    tab: ui::workspace::PaneAuxTabState,   // 이력·Git과 같은 상태 기계
    open: Option<OpenDocument>,
    split_width: Option<f32>,              // source|preview 분할, 이력·Git과 같은 규칙
}

struct OpenDocument {
    path: PathBuf,
    source: String,
    mode: DocumentViewMode,                // Source | Preview | Split
    loaded_revision: DocumentRevision,     // mtime + len + inode
    saved_hash: [u8; 32],
    dirty: bool,
    load_state: DocumentLoadState,
    limit: DocumentLimitTier,              // §6
}
```

- `crates/app/src/ui/document.rs` — 툴바·source 편집기·분할 UX(leaf, intent만 반환)
- `crates/app/src/ui/markdown_viewer.rs` — Viewer facade, page theme, 이미지 broker, link policy
- `crates/app/src/document_io.rs` — 유계 load/save 워커와 revision
- `crates/app/src/app.rs` — 액션 라우팅과 탭 조립만

`check-boundary`가 leaf UI의 `storage::` 접근을 금지한다. 문서 기능은 **DB를 쓰지
않는다** — 최근 문서 목록·커서 위치는 이번 범위에서 저장하지 않는다. 나중에 저장하게
되면 그때 App 소유 projection으로 넣는다(leaf에서 직접 접근 금지).

## 5. 편집기와 Viewer

경량 계획의 결정을 그대로 유지한다.

- source 편집은 egui multiline `TextEdit`를 `SourceEditorAdapter` 뒤에 둔다.
- Viewer는 `egui_commonmark 0.24` 위에 Deppy 소유 facade를 씌운다. 앱의 다른 코드가
  `egui_commonmark` 타입에 직접 의존하지 않는다.
- WebView를 쓰지 않는다.
- raw source만 authoritative state다. Viewer는 재직렬화·저장하지 않는다.

**버전 고정 근거(2026-08-21 실측)**: `egui_commonmark 0.25`와 egui 0.36이 이미 나와
있다. 저장소가 egui 0.35이므로 0.24를 고정한다 — 즉 **egui 업그레이드와 묶인다.**
egui를 0.36으로 올릴 때 `egui_commonmark`도 0.25로 함께 올린다. 이 커플링을 알고
고정한다.

## 6. 한계와 초과 시 동작 — 명시

경량 계획은 1 MiB를 "acceptance tier"로만 적고 **넘으면 어떻게 되는지를 적지
않았다.** 이번 라운드의 dotenv 사고가 정확히 "한계가 전면 차단으로 바뀐" 사례였으므로
(`.env` 한 줄 때문에 워크스페이스에서 세션이 하나도 안 열렸다) 여기서 못박는다.

| 티어 | 조건 | 동작 |
|---|---|---|
| Full | ≤ 1 MiB | 열기·편집·저장·preview 전부 |
| ViewOnly | 1 MiB 초과 ~ 8 MiB | **열린다.** source 읽기 전용 + preview. 편집·저장 잠금, 이유를 툴바에 표시 |
| Refuse | 8 MiB 초과 | 열지 않고 이유를 표시. OS로 열기를 권한다 |
| Binary | UTF-8 아님 | 열지 않고 이유를 표시. OS로 열기를 권한다 |

원칙: **한계 초과는 "조용한 실패"도 "전면 차단"도 아니다.** 할 수 있는 만큼 하고,
못 하는 이유를 그 자리에 보여준다.

## 7. 저장과 외부 변경

- 저장은 같은 디렉터리에 임시 파일을 flush한 뒤 **atomic replace**한다.
- 저장 전 `loaded_revision`을 다시 읽어 비교한다. 다르면 **덮어쓰지 않고** 선택을
  받는다(다시 불러오기 / 다른 이름으로 저장 / 취소).
- 외부에서 바뀐 파일을 자동으로 다시 읽지 않는다. 탭에 표시만 한다.
- 문서 내용과 경로를 로그에 남기지 않는다. 실패 로그는 error code와 바이트 수까지만.

## 8. i18n

사용자에게 보이는 문구는 **5개 로케일 전부** 필요하다(`xtask i18n-check`가 리터럴 키를
강제한다). 필요한 키:

```
workspace.tab.document_hint
workspace.tab.document_close
document.mode.source / document.mode.preview / document.mode.split
document.save / document.saved / document.dirty
document.confirm_discard.title / .save / .discard / .cancel
document.limit.view_only / document.limit.refused / document.limit.binary
document.conflict.title / .reload / .save_as / .cancel
document.error.load_failed / document.error.save_failed
```

## 9. 구현 순서

경량 계획의 L0가 크게 줄었다 — outer split을 만들지 않고 기존 보조 탭에 얹기 때문이다.

### D0. 탭 자리 만들기 (0.5일)
- `PaneAuxTabKind::Document` 추가, `PANE_AUX_TAB_MAX` 2 → 3, 축약 순서(§2) 구현
- `SidebarAction::OpenDocument`와 App 라우팅
- 빈 문서 표면을 열고 닫아도 터미널 split/focus/입력이 그대로인지 검증

### D1. 로컬 source 편집 (1-2일)
- UTF-8 load, `TextEdit` source 모드, dirty/저장/닫기 확인
- revision 충돌과 atomic save
- 한글 IME·클립보드·undo/redo 확인

### D2. Markdown Viewer (2-3일)
- `MarkdownViewer` facade, page theme, 이미지 broker, link policy, render cache
- Source / Preview / Split 토글과 분할 폭 기억

### D3. 마감 (1-2일)
- §6 티어 4종의 실제 동작과 문구
- 큰 파일·깨진 UTF-8·외부 변경 fixture

## 10. 검증

### 자동
- 탭 상태 기계: 열기/활성/비활성/닫기, 세션 탭 클릭 복귀
- 헤더 폭 40~600pt 스윕 — 어떤 rect도 툴바/헤더 경계를 넘지 않고 라벨이 0폭이 되지 않는다(이력 탭이 쓰던 것과 같은 방식)
- 문서 탭 활성 중 터미널 입력이 **차단**되는지(fail-closed)
- 보조 탭에서 `RuntimeCommand`가 파생되지 않는지 — 문서 `×`는 pane을 닫지 않는다
- §6 티어 판정의 경계값(1 MiB, 8 MiB, 비 UTF-8)
- revision 충돌 시 덮어쓰지 않는지
- `xtask i18n-check`, `check-boundary`

### 수동
- 한글 IME 조합 중 저장, 큰 md의 preview 스크롤 성능, split 폭 드래그

## 11. 이번 범위에서 제외

원격 파일, 범위 주석, LSP, multi-cursor, slash command, wikilink 색인, 최근 문서
영속화. 편집기 엔진 경쟁(CodeMirror/Monaco)은 `document-editor-development-plan.md`가
이어받는다.

**2026-08-22 갱신**: "문서 탭 여러 개 동시 표시"는 이 범위 제외 목록에서 빠졌다 —
별도 라운드(멀티 문서 탭 설계)에서 구현했다. `PaneAuxTabKind::Document`가
`DocumentTabId`를 실어 문서별로 식별하고, App은 `document: Option<OpenDocument>`
대신 `documents: Vec<OpenDocument>` + `active_document`를 소유한다. §2의 탭 상한
(`PANE_AUX_TAB_MAX`)과 §3.3의 "pane당 문서 하나(교체)" 규칙은 이 갱신으로
대체됐다 — 상한은 이제 개수 상한이 없는 축약 사다리(활성 탭은 절대 버리지
않는다) + App 쪽 `DOCUMENT_TABS_MAX`·`DOCUMENT_TOTAL_RETAINED_BYTES_MAX`가 맡고,
새 문서를 열어도 기존 문서를 교체하지 않는다(가득 차면 가장 오래된 clean 비활성
문서를 닫아 자리를 만든다).
