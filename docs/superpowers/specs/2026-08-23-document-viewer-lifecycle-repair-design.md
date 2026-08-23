# 문서 Viewer 수명주기·저장 경계 보수 설계

상태: 2026-08-23 사용자 승인, 구현 전

선행 문서:

- `docs/superpowers/specs/2026-08-21-document-tab-design.md`
- `docs/document-editor-lightweight-core-plan.md`

## 0. 목적과 실측 근거

이 라운드는 문서 기능을 다시 설계하지 않는다. 실제 프로덕션 `MarkdownViewer`와
`document_io`를 사용한 계측에서 재현된 수명주기·저장 경계 문제만 고친다.

- Split 세로 스크롤 offset `180`은 다음 source revision에서 `0`으로 초기화됐다.
  100 revision은 persisted 세로 상태 100개를 남겼다.
- Preview와 Split의 바깥 가로 ScrollArea ID는 서로 달랐다. 닫기 전 네 개였던
  스크롤 상태는 `forget_document` 뒤 세 개로만 줄어 이전 모드 상태가 남았다.
- 링크 훅 1,000개는 문서를 닫아도 그대로였다. 100,000개에서는 8,725,384 B가
  남고 `prepare_show`가 5회 중앙값 116.052 us/frame을 썼다.
- 실제 1024x1024 PNG 한 장은 닫은 뒤에도 encoded 22,648 B, decoded 4,194,304 B,
  loader-reported texture 4,194,304 B가 다음 Preview까지 남았다. 이는 무한 누적이
  아니라 현재 한 generation의 지연 해제다.
- 8 MiB+1 source 저장은 worker가 `ContentTooLarge`로 거부하기 전에 같은 크기의
  `String`을 두 번 clone해 16,777,218 B를 임시로 더 할당했다. 정확히 8 MiB는
  정상 저장됐다.
- `OpenDocument::body_ui_id`는 일곱 occurrence 중 read가 하나도 없는 dead state다.
- HEAD~1/HEAD의 repaint·Viewer 호출 수와 프레임 경로는 같았다. 중복 repaint/render는
  재현되지 않았으므로 이 설계의 수정 대상이 아니다.

## 1. 목표와 비목표

### 목표

1. source revision이 바뀌어도 같은 문서의 세로 스크롤 위치를 보존한다.
2. revision·문서·모드 전환 및 닫기 뒤에 persisted 스크롤 상태와 링크 훅이
   무한히 늘지 않게 한다.
3. 현재 문서를 닫으면 그 문서의 CommonMark 캐시와 이미지 자원을 즉시 해제한다.
4. 8 MiB 초과 편집 내용은 보존하되 저장을 명시적으로 막고, 본문 clone 전에
   거부한다. 다시 8 MiB 이하가 되면 저장을 자동 복구한다.
5. `body_ui_id` dead path를 제거한다.

### 비목표

- Preview debounce 추가 또는 같은 프레임 Split Preview 제거
- repaint 예약 경로 변경
- `egui_commonmark` 포크·업그레이드
- 문서별 `MarkdownViewer` 인스턴스 도입
- 이미지 상한, 문서 로드 티어, 탭 구조 변경
- 정상 범위 저장의 snapshot correctness를 지키는 두 소유본 구조 변경

## 2. 선택한 접근

하나의 `MarkdownViewer` facade를 유지하면서 **persisted UI ID**와 **렌더 캐시
무효화 key**를 분리한다. 전역 Viewer 초기화는 다른 문서의 정상 상태까지 버리고,
문서별 Viewer는 최대 여덟 문서에 캐시 소유 구조를 중복시키므로 채택하지 않는다.

이 접근은 `crates/app/src/ui/markdown_viewer.rs`와 문서 저장/확인 경계만 건드리는
외과적 변경이다. 현재 source authoritative state, 같은 프레임 Split Preview,
링크 intent, 이미지 보안 검증은 그대로 둔다.

## 3. Viewer 상태 설계

### 3.1 세로 스크롤: 안정 ID와 무효화 signature 분리

현재 `ScrollCacheKey { slot, revision, dark_mode, width_bucket }`를
`show_scrollable`의 `source_id`로 그대로 넘겨 UI 상태와 렌더 캐시가 같은 수명을
갖는다. 이를 두 역할로 나눈다.

- 안정 `source_id`: `MarkdownDocumentSlot`만으로 만든 절대 `egui::Id`
- 렌더 signature: `slot + revision + dark_mode + width_bucket`

signature가 바뀌면 안정 `source_id`에 대응하는 CommonMark scrollable cache만
`clear_scrollable_with_id`로 비운 뒤 같은 `source_id`로 다시 렌더한다. 이 호출은
CommonMark의 split-point/page-size 캐시만 지우고 egui의 persisted ScrollArea state는
지우지 않으므로, 새 source를 파싱하면서 offset은 유지된다.

문서를 닫을 때는 둘 다 지운다.

1. 안정 `source_id`의 CommonMark scrollable cache 제거
2. 업스트림이 만드는 `source_id.with("_scroll_area")` persisted state 제거
3. 현재 signature가 그 slot이면 signature를 `None`으로 초기화

### 3.2 가로 스크롤: slot당 실제 ID 전부 소유

가로 ScrollArea의 실제 ID는 parent UI를 포함하므로 Preview와 Split에서 다르다.
`HashMap<slot, Id>`의 마지막 값 하나만 저장하지 않고
`HashMap<slot, HashSet<Id>>`로 관측된 실제 ID를 모두 보관한다.

문서를 닫을 때 해당 slot의 set을 꺼내 모든 persisted `ScrollArea::State`를 제거한다.
동시에 열 수 있는 표시 모드는 하나이고 parent 종류도 Preview/Split로 유계이므로,
정상 사용에서 set은 작은 상수 크기에 머문다.

### 3.3 링크 훅: 현재 문서 집합으로 교체

매 `show`에서 링크를 등록하기 전에 `CommonMarkCache::link_hooks_clear()`를 호출한 뒤
현재 `destinations`의 link target만 다시 등록한다. Viewer가 한 프레임에 그리는 것은
활성 문서 Preview 하나뿐이므로 이전 문서나 이전 revision의 훅을 유지할 이유가 없다.
클릭 판정은 같은 `show` 호출이 훅 값을 설정한 뒤 읽으므로 동작 순서는 변하지 않는다.

닫힌 slot이 `destinations_key`가 가리키는 현재 문서일 때도 훅을 비워 Preview가 없는
프레임 동안 문자열을 붙잡지 않게 한다. background slot 정리는 active 문서의 훅을
건드리지 않는다.

### 3.4 이미지 broker와 현재 CommonMark 자원 정리

`WorkspaceImageBroker`에 slot-aware `forget_document(ctx, slot)`을 둔다.
`last_generation`의 slot이 닫힌 문서와 같을 때만 다음을 수행한다.

1. `registered_uris`를 drain하며 `ctx.forget_image(uri)` 호출
2. `last_generation = None`

다른 문서가 현재 표시 중이면 그 문서 자원을 건드리지 않는다. `MarkdownViewer`의
`forget_document`는 target slot의 안정 세로 ID state와 모든 가로 state를 항상
정리한다. CommonMark cache/signature, destinations, 링크 훅, image broker처럼 facade
전체에서 하나뿐인 현재 자원은 각각의 current key/generation이 target slot과 일치할
때만 정리한다.

## 4. 8 MiB 저장 경계

승인된 제품 동작은 다음과 같다.

- Full 티어로 연 문서가 편집 중 8 MiB를 넘어도 source를 자르거나 편집기를 잠그지
  않는다.
- 저장 버튼은 비활성화하고 툴바에 "내용을 줄이면 다시 저장할 수 있다"는 이유를
  표시한다.
- dirty 닫기 확인창에서도 저장 버튼을 비활성화하고 같은 이유를 보여준다. 버리기와
  취소는 계속 가능하다.
- source가 정확히 8 MiB이면 저장 가능하다.
- 편집으로 다시 8 MiB 이하가 되면 별도 조작 없이 저장 가능 상태로 돌아온다.

`OpenDocument::can_save`는 기존 `dirty && !saving && Full`에 byte 경계를 더한다.
UI의 비활성화만 신뢰하지 않고 `request_document_save`도 먼저 기본 자격
(`dirty && !saving && Full`)을 확인한 뒤 `source.len()`을 clone보다 먼저 검사한다.
따라서 request 함수는 size까지 포함한 `can_save == false`만 보고 곧장 return해서는
안 된다. 초과면 다음 상태를 보장한다.

- worker request를 queue에 넣지 않는다.
- `saving_source`를 만들지 않는다.
- `saving`을 시작하지 않는다.
- `save_error = ContentTooLarge`로 명시한다.
- 혹시 dirty-close의 save continuation이 먼저 설정돼 있었다면 제거해 탭이 나중에
  잘못 닫히지 않게 한다.

바이트 기준은 `String::len()`과 `DOCUMENT_REFUSE_BYTES_MAX`를 사용해 worker의 기존
검사와 정확히 일치시킨다. 사용자 문구는 다섯 locale에 같은 의미로 추가한다.

## 5. dead state 제거

`OpenDocument::body_ui_id`, render 중 local capture/대입, production/test initializer,
절대 TextEdit ID 이전 설계를 설명하는 구식 주석을 제거한다.
`clear_document_editor_state`는 현재처럼
`document_source_editor_id(&document.path)`만 사용한다.

## 6. 오류·경계 동작

- 존재하지 않거나 이미 닫힌 slot의 정리는 no-op이다.
- 같은 문서를 여러 번 정리해도 panic하지 않는다.
- background 문서를 닫아도 현재 active 문서의 image/cache를 지우지 않는다.
- revision 변경은 scroll offset을 보존하지만, 문서 닫기 후 같은 경로를 다시 열면
  이전 offset을 복원하지 않는다.
- 저장 초과는 source를 변경하거나 디스크에 쓰지 않는다.
- 저장 초과 뒤 source를 줄이면 기존 `on_document_source_edited`가 오류를 지우고
  `can_save`가 다시 true가 된다.
- 링크·경로·문서 내용은 로그에 남기지 않는다.

## 7. 검증 설계

구현은 RED/GREEN 순서로 진행한다.

### Viewer 회귀

1. 같은 slot에서 revision을 100번 바꿔도 persisted 세로 state는 하나이고 기존
   offset이 유지된다.
2. Preview와 Split parent에서 얻은 두 가로 ID를 모두 기록한 뒤 닫으면 두 state가
   모두 사라진다.
3. revision마다 고유 링크를 넣어도 hook map은 현재 source 링크 수를 넘지 않고,
   닫은 뒤 0이다.
4. 실제 PNG를 include/decode한 뒤 닫으면 URI bytes, decoded image, texture loader
   조회와 broker URI가 모두 사라진다.
5. 현재 표시 중이 아닌 slot 정리는 active slot 자원을 보존한다.
6. `forget_document`를 두 번 호출해도 안전하다.

### 저장 회귀

1. 정확히 8 MiB는 `can_save`와 worker 저장이 모두 성공한다.
2. 8 MiB+1은 toolbar/닫기 확인 저장이 비활성이고 request queue와
   `saving_source`가 생기지 않는다.
3. 방어적 request 호출도 clone/worker admission 전에 `ContentTooLarge`로 끝나며
   close-after-save continuation을 제거한다.
4. 초과 상태에서 source를 8 MiB 이하로 줄이면 저장이 다시 활성화된다.
5. 초과 저장 시 디스크 내용은 바뀌지 않는다.

### 마감 gate

- `cargo test -p deppy-sijo --locked document -- --test-threads=1`
- `cargo test -p deppy-sijo --locked markdown_viewer -- --test-threads=1`
- `cargo clippy -p deppy-sijo --all-targets --locked -- -D warnings`
- `cargo fmt --all -- --check`
- `cargo run -p xtask --locked -- i18n-check`
- `cargo run -p xtask --locked -- check-boundary`
- `git diff --check`

성능 시간 자체를 CI 임계값으로 두지 않는다. 대신 누적 원인인 state/hook/resource
개수를 직접 검증해 머신 속도와 무관한 회귀 테스트로 고정한다.

## 8. 완료 조건

- 실측으로 확인한 네 defect group과 dead state만 수정된다.
- 100 revision 뒤 state/hook 수가 source history에 비례하지 않는다.
- 닫힌 현재 문서의 이미지와 CommonMark/egui 스크롤 자원이 남지 않는다.
- 8 MiB+1 저장은 source clone이나 worker request 없이 거부되고 내용은 보존된다.
- 정확히 8 MiB 저장, 같은 프레임 Split Preview, 기존 링크 intent와 이미지 보안
  정책은 유지된다.
- repaint/multi-pass 경로에는 변경이 없다.
