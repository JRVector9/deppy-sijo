# 리뷰 교정 및 파일 트리 한글 표시 설계

상태: 2026-08-24 사용자 승인, 구현 전

선행 설계:

- `docs/superpowers/specs/2026-08-24-pane-resize-flicker-elimination-design.md`
- `docs/superpowers/specs/2026-08-21-document-tab-design.md`

## 1. 목표

최종 코드 리뷰에서 재현된 네 상태·렌더 결함과 실제 macOS 파일명에서 재현된 한글
표시 결함을 최소 범위로 교정한다.

1. 저장 중인 문서를 닫을 때 화면 내용과 디스크 내용이 달라지는 상태 전이를 막는다.
2. 세션으로 이동할 때 문서·Git·히스토리 보조 탭을 비활성화해 터미널을 실제로 보인다.
3. 이전 resize presentation fence가 다음 divider drag 중 snapshot을 승격하지 못하게 한다.
4. divider가 사라진 동안 pointer release가 발생해도 split transaction이 고착되지 않게 한다.
5. 폴더 트리의 macOS NFD 한글 파일명을 NFC로 합성해 표시하되 실제 경로는 바꾸지 않는다.

새 기능이나 전역 상태 기계를 추가하지 않는다. 기존 저장 continuation, 보조 탭 전이,
split transaction, resize presentation, file-tree flat cache를 재사용한다.

## 2. 확인된 원인과 실측

### 2.1 저장 중 닫기

`document_close_requires_confirm`은 `dirty`만 본다. 디스크/화면이 `A`인 문서를 `B`로
편집해 저장을 시작한 뒤, 저장 완료 전에 화면을 다시 `A`로 되돌리면 `dirty=false`지만
worker의 `saving_source`는 `B`다. 현재 닫기 경로는 탭을 즉시 제거하고 worker는 디스크에
`B`를 쓴다. 늦은 결과는 제거된 문서를 찾지 못해 버려진다.

기존 `document_close_after_save`와 저장 결과 적용 경로는 저장 완료 후 `saved_source`를
갱신하고 현재 `source`와 다시 비교할 수 있으므로 이 continuation을 재사용한다.

### 2.2 세션 이동과 보조 탭

`reveal_terminal_session()`은 history만 `on_session_tab_click()`으로 바꾼다. 활성 document
또는 Git 탭은 그대로여서 중앙 view가 Terminal이어도 보조 본문이 계속 렌더되고 terminal
input owner도 비활성이다. 세 보조 탭 모두 이미 `OpenActive -> OpenInactive` 전이를
지원하므로 문서나 Git 모델을 삭제할 필요는 없다.

### 2.3 resize fence와 다음 drag

split 렌더는 두 child pane을 먼저 렌더하고 마지막에 divider response를 등록한다. 각
`render_pane`은 이전 `resize_presentation`을 정착시킬 수 있으므로, 다음 drag의 첫 프레임은
transaction이 활성화되기 전에 이전 candidate snapshot이 승격될 수 있다. 이후 active drag
프레임에도 fence settlement가 무조건 실행된다.

### 2.4 사라진 divider의 active transaction

Active transaction의 정상 종료는 divider `Response::drag_stopped()`에 의존한다.
`input_enabled=false`, 다른 view/tab 전환, window hide처럼 divider가 렌더되지 않는 동안
release되면 stop을 관찰하지 못한다. Active 상태는 mux ACK 대상도 아니고 모든 terminal
resize staging을 차단하므로 복귀 후에도 preview와 resize가 고착된다.

### 2.5 macOS 한글 파일명

실제 경로
`/Users/jr/Desktop/projects/colon35/Design/화면 디자인/Market 작품목록 (standalone).html`
의 한글은 파일시스템에서 `U+110C U+1161 U+11A8 ...` 형태의 NFD 자모로 반환된다.
`file_tree.rs`는 raw `entry.file_name()`을 `TreeNode`, `FlatRow.name`, `RichText`까지 그대로
전달한다. Apple SD Gothic은 이 문자열을 screenshot처럼 분리 자모로 표시한다.

`FlatRow.name`의 production 소비처는 라벨과 색상뿐이다. 열기, 펼침, 이동, DnD, 복사,
삭제, `cd`, rename 초기값은 모두 raw `FlatRow.path` 또는 그 `file_name()`을 사용한다.

## 3. 선택한 아키텍처

### 3.1 App 상태 전이 교정

문서 닫기 판정은 다음 세 결과로 고정한다.

```text
saving=true  -> DeferUntilSave
saving=false, dirty=true -> ConfirmDirty
saving=false, dirty=false -> CloseNow
```

저장 중 닫기는 문서를 제거하지 않고 기존 `document_close_after_save`에 ID를 기록한다.
저장 성공 뒤 현재 source가 저장 snapshot과 같으면 닫고, 다르면 dirty 재계산 결과를 바탕으로
확인창을 다시 표시한다. 실패나 충돌은 기존 오류/충돌 UI를 유지하고 탭을 남긴다. 진행 중인
파일 쓰기를 취소할 수 없으므로 저장 중 즉시 Discard 닫기는 제공하지 않는다.

세션 이동은 history, Git, document 세 상태에 `on_session_tab_click()`을 적용한다. 하나라도
활성 보조 표면이었다면 보조 검색 상태를 초기화한다. 탭과 문서 모델은 삭제하지 않는다.

### 3.2 Split transaction과 presentation 경계

divider widget ID는 생성과 lifecycle reconciliation이 같은 helper를 사용한다. split child를
렌더하기 전에 egui의 현재 drag owner와 pointer 상태를 읽어 해당 path의 Active transaction을
시작하거나 preview ratio를 갱신한다. 실제 divider `interact` 등록은 기존 렌더 순서를 유지해
hit-test 우선권을 바꾸지 않는다.

`split_drag`가 존재하거나 session이 final resize transaction의 일부인 동안 이전
`resize_presentation` settlement를 보류한다. transaction 종료/ACK 뒤의 정상 프레임에서
기존 quiet/deadline 규칙으로 다시 정착시킨다. drag 중 별도 timer나 snapshot 복사를 만들지
않는다.

`show_with_input` 진입에서는 Active transaction과 실제 입력 소유권을 조정한다.

- 같은 divider의 stop이 관찰되면 기존 commit 경로를 사용한다.
- input owner 상실, active tab/path 불일치, widget 부재 중 release가 확인되면 cancel한다.
- cancel은 persisted mux ratio를 바꾸거나 `ResizeSplit`을 보내지 않는다.
- Committed transaction의 ACK 수명은 기존 로직을 유지한다.
- cancel 뒤 terminal resize staging은 다음 정상 pass부터 다시 허용한다.

### 3.3 파일 identity와 표시 문자열 분리

`TreeNode.name`, snapshot 정렬, hidden filtering, node 탐색은 raw NFD를 유지한다.
`flatten()`은 먼저 `base.join(&node.name)`으로 raw `PathBuf`를 만든 뒤, 표시 문자열만
`UnicodeNormalization::nfc()`로 합성한다. `FlatRow.name`은 `display_name`으로 명명해
filesystem identity에 재사용되지 않게 한다.

정규화는 `rebuild_flat()` 때만 실행된다. 매 프레임 렌더 할당은 없고, 기존 FlatRow String을
대체하므로 보관 문자열 수도 늘지 않는다. app crate의 기존 direct dependency를 사용해
manifest는 변경하지 않는다.

## 4. 채택하지 않은 접근

### 4.1 파일 listing 경계에서 이름 전체 정규화

표시는 해결되지만 같은 문자열이 path 재구성, 정렬, node 탐색에도 사용되어 filesystem
identity를 바꾼다. 원본 경로 보존 계약과 맞지 않아 제외한다.

### 4.2 렌더 프레임마다 NFC 정규화

수정은 작지만 visible row마다 매 프레임 문자열 순회와 할당이 발생한다. 스크롤/resize 중
불필요한 churn을 만들기 때문에 flat cache 경계를 선택한다.

### 4.3 App/Workspace 전역 상태 기계 재작성

각 결함에는 이미 올바른 continuation과 transaction 타입이 있다. 큰 리팩터링은 저장,
document lifecycle, split ACK의 기존 검증 범위를 넓혀 회귀 위험만 키우므로 제외한다.

## 5. 오류·중단 계약

- 저장 실패나 충돌 시 닫기 intent를 강제로 완료하지 않는다.
- 보조 탭 전환은 사용자 내용을 삭제하거나 저장 상태를 변경하지 않는다.
- split cancel은 마지막 persisted ratio를 유지하고 host command를 보내지 않는다.
- 정규화 불가능한 별도 오류는 없다. Rust `str`인 listing display에 NFC iterator만 적용한다.
- lossy filename 처리와 listing 자원 상한 정책은 기존 동작을 유지한다.

## 6. 성능·메모리·repaint 계약

- 저장 continuation에 새 worker, queue, content copy를 추가하지 않는다.
- drag 중 snapshot을 새로 복제하지 않고 기존 stable presentation을 유지한다.
- split lifecycle 조정은 입력/렌더 프레임에서만 수행하며 idle polling을 추가하지 않는다.
- file-tree NFC 변환은 flat cache 재구성 때 항목당 한 번이며 매 프레임 실행하지 않는다.
- raw tree retained-byte 상한과 listing-byte 상한은 변경하지 않는다.

## 7. 병렬 구현 경계

충돌을 막기 위해 세 exclusive lane으로 실행한다.

1. App lane: `crates/app/src/app.rs`만 수정한다.
2. Workspace lane: `crates/app/src/ui/workspace.rs`만 수정한다.
3. File-tree lane: `crates/app/src/ui/file_tree.rs`만 수정한다.

subagent는 manifests, docs, handoff를 수정하지 않는다. root agent가 설계/계획/통합과 최종
리뷰를 소유한다. 공유 worktree에서 각 lane의 production edit 전에 반드시 해당 regression
test를 추가하고 예상한 RED를 실제 실행한다.

## 8. 테스트 설계

### 8.1 App lane RED

- saving=true, dirty=false 닫기 요청이 즉시 문서를 제거하지 않고 저장 완료를 기다린다.
- 저장 결과가 현재 화면과 다르면 dirty가 다시 true가 되고 닫기 확인으로 이어진다.
- 저장 결과와 닫기 입력 순서가 바뀌어도 데이터 보존 결과가 같다.
- document active 및 Git active 각각에서 세션 이동 후 모든 보조 탭이 inactive다.
- Closed 보조 탭과 문서/Git 모델은 그대로 유지된다.

### 8.2 Workspace lane RED

- 이전 fence가 promote-ready여도 새 drag 첫 프레임에는 stable snapshot이 유지된다.
- Active split transaction 동안 이전 fence settlement가 보류된다.
- divider가 없는 프레임에서 release한 뒤 복귀하면 transaction이 cancel된다.
- cancel은 `ResizeSplit`을 보내지 않고 이후 terminal resize staging을 다시 허용한다.

### 8.3 File-tree lane RED

NFD `화면 디자인` 노드 하나를 flatten해 동시에 검증한다.

- `TreeNode.name`은 raw NFD다.
- `FlatRow.display_name`은 NFC `화면 디자인`이다.
- `FlatRow.path`는 raw NFD 경로이며 NFC로 만든 별도 경로와 다르다.

### 8.4 통합 회귀

- 각 focused test를 GREEN으로 재실행한다.
- Workspace 전체 unit group과 document/App 관련 group을 직렬 실행한다.
- file-tree group을 실행한다.
- `cargo fmt --all -- --check`, `git diff --check`, 프로젝트 boundary/i18n gate를 실행한다.
- 변경 코드 diff를 `codex review --uncommitted`로 검토하고 Critical/High 및 범위 내 Medium을
  반영한 뒤 focused/regression gate를 다시 실행한다.

## 9. 완료 조건

- 실제 NFD 한글 파일명은 조합된 한글로 보이고 모든 파일 작업은 원본 경로를 사용한다.
- 저장 중 닫기 재현에서 화면/디스크 불일치가 생기지 않는다.
- 세션 이동 뒤 터미널 본문과 입력이 활성화되고 보조 탭 내용은 보존된다.
- 반복 split drag와 중간 view 전환에서 stale snapshot 승격이나 고착 transaction이 없다.
- 모든 새 회귀 테스트가 RED를 거쳐 GREEN이 되고 통합 gate와 최종 코드 리뷰가 완료된다.
