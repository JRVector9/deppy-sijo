| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | app.rs:32661 | 다음 문서로 Discard 포커스가 승계됨 | 다음 문서의 미저장 내용도 Enter로 버릴 수 있음 | 문서별 버튼 ID와 대상 전환 시 안전한 포커스 |
| high | app.rs:32023 | 확인창 최초 프레임에 배경 입력 허용 | 저장 충돌 안내가 뜨면서 PTY 입력이 전달됨 | 대기 확인 상태로 렌더 전 입력 차단 |
| high | ui/workspace.rs:9959 | 모든 Foreground를 차단 팝업으로 간주 | 검색 Enter/Esc와 인라인 이름 변경 실패 | 실제 모달의 상태와 소유권으로 판별 |
| medium | storage/db.rs:6889 | 다른 장치의 같은 inode를 동일 폴더로 인정 | 다른 볼륨을 기존 워크스페이스에 연결 가능 | 안정적 볼륨 식별자 또는 변경 확인 |
| medium | ui/popup/actions.rs:119 | 긴 버튼을 한 행에 강제 배치 | 좁은 창에서 취소 버튼 일부 잘림 | 버튼 합계 폭에 따른 줄바꿈/세로 배치 |

# 구현된 팝업 전체 코드 리뷰 — 2026-09-30

## 검토 범위와 상태

- 실제 구현 worktree: `/Users/jr/Desktop/projects/deppy-sijo-performance`.
- Branch `fix/cloud-agent-ended-sessions`, base HEAD `166f8daeb1054cf09a07194fa83bf0a4a19d93ce`, 기존 미커밋 제품 변경 포함 **0.4.10 소스**.
- 공용 팝업 **13개 사례**: 01·02·03·07·08·10·11·12·13·26·27·28·29. 셸·필드·안내·버튼·확인 렌더러, App/파일 트리/워크스페이스/리소스/포트 호출부, 비동기 접수·결과·취소, 입력 차단과 경로 등록을 검토했다. 04 인라인 이름 변경, Fleet/프롬프트/Connector의 겹침 및 Esc 처리도 연관 범위에 포함했다.
- 번호가 있는 HTML 42개 사례가 모두 공용 팝업으로 구현됐다고 간주하지 않았다. 전용 AI 런처와 OS 기본 창은 공용 셸 대상이 아니다.
- `codex-reviewer` 방식으로 독립 CLI와 동작/구조·성능/메모리·보안/대상·품질 리뷰를 교차 확인했다. 제공되지 않는 Sonnet 대신 현재 Codex 리뷰어를 사용했다. 보안 리뷰어가 증거를 전달한 뒤 모델 capacity 오류로 최종 요약을 끝내지 못해 root가 재현과 나머지 검증을 완료했다.
- 아래 **5건은 미수정**이다. 이번 작업은 리뷰/보고이며 제품 코드·버전·앱 번들을 변경하거나 Deppy를 실행하지 않았다. 수정된 문서는 이 보고서와 `docs/CODEX_HANDOFF.md`다.

## Details

### F1 · high · 문서 큐에서 위험 버튼 포커스 승계

**위치:** `crates/app/src/app.rs:32661`, `:32677`; `crates/app/src/ui/document_dialogs.rs:32`.

미저장 닫기/저장 충돌 팝업은 문서가 바뀌어도 같은 Modal ID를 쓴다. 하단 버튼의 자동 widget ID 역시 유지되고, 문서 전환 시 focus를 해제하거나 안전한 동작으로 옮기는 처리가 없다. 첫 문서에서 키보드로 선택한 Discard가 다음 문서에서도 선택된 상태다.

실제 `dirty()`와 egui_kittest를 쓰는 외부 하네스에서 두 문서를 차례로 표시했다. 첫 문서 Discard에 명시적으로 focus를 주고 Enter를 누른 뒤, 다음 문서에서는 추가 focus 선택 없이 Enter만 눌렀다.

```text
after explicit focus and first Enter: (1, [("first.md", Discard)])
after bare Enter on second document: (2, [("first.md", Discard), ("second.md", Discard)])
```

이는 사용자가 두 번째 문서의 위험 동작을 새로 선택하지 않아도 미저장 내용을 버릴 수 있는 문제다. 저장 충돌 팝업도 동일한 ID/버튼 구조지만, 위 출력은 26 실제 재현이다. 모든 종료 팝업에서 재현됐다고 확대하지 않는다.

**수정 방향:** Area ID는 사례별로 제한해 유지하되 문서 대상별 child/widget ID를 부여하고, 큐의 대상이 변경되면 이전 focus를 지우거나 Cancel로 이동한다. 회귀 테스트는 실제 팝업을 연속 표시하고 다음 문서의 bare Enter가 Discard/Reload를 반환하지 않는지 확인해야 한다.

### F2 · high · 비동기 확인창 최초 프레임의 터미널 입력 누출

**위치:** `crates/app/src/app.rs:32023` (`:31978`도 같은 조건); `:19183`, `:32675`; `crates/app/src/ui/workspace.rs:7774`, `:8110`.

`logic()`의 `poll_document_io()`가 저장 Conflict 결과를 받아 확인 큐를 채운다. 사용자가 저장 대기 중 터미널 탭으로 돌아간 상태라면 App은 확인 큐와 무관하게 `show_with_input(..., true)`를 호출한다. WorkspaceUi는 egui에 등록된 모달/영역만 확인한다. 해당 프레임에서는 문서 Modal을 아직 그리지 않았고, App이 나중에 그려 최초 키 이벤트가 이미 `RuntimeCommand::WriteInput`이 된다.

root가 App과 같은 순서로 **실제 WorkspaceUi와 실제 conflict()**를 호출하는 화면 밖 하네스로 재현했다. 런타임 명령은 하네스가 수집하고 완료시킬 뿐 실제 PTY/셸에 실행하지 않는다. 프롬프트가 없는 baseline에서 입력을 먼저 확인하고, 다음 프레임에 확인 상태와 입력 이벤트를 함께 공급했다. 다음 프레임 이후 모달 차단이 작동하는지도 확인했다.

```text
fresh conflict modal first-frame WriteInput: [[114, 97, 99, 101, 13]]
subsequent visible-modal input was blocked
```

위 바이트는 `race\r`다. 텍스트와 Enter가 같은 첫 프레임에서 터미널 입력 명령으로 접수됐다는 증거이며, 실제 셸에서 이 명령을 실행한 테스트는 아니다.

**수정 방향:** egui 레이어가 생기기 전부터 App의 대기 확인 상태로 터미널·검색·전역 키 입력을 차단한다. 주/부착 surface의 모든 입력 소유자에 같은 프레임 정책을 전달한다. 모달을 그린 다음 프레임만 검사하는 기존 테스트로는 부족하다. 이 호출 순서 문제를 이번 변경으로 새로 생긴 회귀라고 단정하지 않는다.

### F3 · high · Foreground 영역 전체 차단으로 검색/인라인 편집 회귀

**위치:** `crates/app/src/ui/workspace.rs:9959`; 연관 `:4408`, `:4559`, `crates/app/src/ui/popup/mod.rs:30`.

터미널 검색은 일반 `Order::Foreground` Area다. 새 입력 차단 조건이 Foreground 전체를 차단 창으로 판정하므로 검색창 자체가 `handle_terminal_search_keys`의 early return 원인이 된다. Enter/Shift+Enter로 결과 이동, Esc로 닫기가 멈추고 터미널로 focus를 돌려도 검색 Area가 남은 동안 배경 입력/단축키가 차단된다.

또한 `visible_layer_ids()`에는 바로 닫힌 context menu의 이전 프레임 레이어가 남을 수 있다. 우클릭 메뉴에서 Rename을 선택해 메뉴가 닫혔는데도 첫 인라인 편집 프레임이 이 조건으로 취소된다. 세션/워크스페이스 이름 변경은 같은 원인이므로 한 건으로 합쳤다.

독립 CLI가 실제 검색 key handler와 현재 차단 predicate를 추출한 offscreen probe를 실행했고 root도 출력 확인 후 재실행했다. 동일 하네스에서 predicate만 HEAD의 이전 구현으로 바꾼 비교 결과다.

```text
현재: Enter handled=false, current=Some(0)
현재: Escape handled=false, search_exists=true
이전 predicate: Enter handled=true, current=Some(1)
이전 predicate: Escape handled=true, search_exists=false

source menu open: false, memory any popup: false
현재: inline editor result = Some(Cancel)
이전 predicate: inline editor result = None
```

**수정 방향:** 실제 차단 모달의 현재 상태/ID를 사용한다. 검색 Area와 해제된 팝오버를 구분하며 `take_window_escape`의 같은 blanket check도 정리해야 한다. Foreground만 제외하면 최초 프레임 차단이 다시 빠질 수 있으므로 F2와 함께 입력 정책을 수정한다.

### F4 · medium · 폴더 등록 backend의 볼륨 식별 약화

**위치:** `crates/storage/src/db.rs:6889`; 호출 `crates/app/src/app.rs:4788`.

01에서 선택한 로컬 폴더는 stat의 `(dev, ino)`를 갖고 `find_or_create_workspace_by_exact_path`에 들어간다. 바뀐 구현은 경로가 같으면 inode만 같아도 기존 ID/이름/생성 시각을 재사용하고 저장된 device를 덮어쓴다. 재부팅/재마운트로 device 번호가 변한 같은 폴더와, 같은 경로에 다른 볼륨을 연결해 같은 inode 번호가 나온 경우를 구분하지 못한다. inode 번호만으로 서로 다른 장치의 동일 디렉터리를 증명할 수 없다.

현재 테스트 `workspace_find_or_create_refreshes_remounted_device_at_same_path`는 `(11,22)→(33,22)`에서 ID 재사용/anchor 갱신을 실제로 확인하고 통과했다. 따라서 허용 동작은 검증됐다. 실제 다른 사용자 볼륨을 마운트하거나 사용자 DB를 변경한 테스트는 수행하지 않았고, 다른 볼륨에서의 충돌은 위 조건과 식별 범위에 따른 코드 분석이다. 공용 렌더러 문제가 아닌 폴더 열기 연관 backend 결함으로 분리했다.

**수정 방향:** 볼륨 UUID 등 재마운트에도 유지되는 식별자와 inode를 함께 검증한다. 안정적 식별 정보가 없는 기존 행의 device 변경은 새 폴더로 처리하거나 명시적 재연결 확인을 거친다. 이름/기록/기존 세션을 조용히 재사용하지 않도록 별도 볼륨 충돌 회귀가 필요하다.

### F5 · medium · 좁은 화면의 긴 번역 버튼 잘림

**위치:** `crates/app/src/ui/popup/actions.rs:119`.

공용 footer는 모든 action을 줄바꿈 없는 RTL 행으로 그린다. shell은 선호 폭을 화면 폭에 맞추지만 버튼의 자연 폭 합계가 가용 폭보다 크면 행/Area가 확장된다. 영어 27의 `Reload and discard edits`와 Cancel에서 문제를 실제 측정했다.

실제 공용 컴포넌트/문서 표시 함수, en-US, 280×500pt 하네스:

```text
Modal bounds: x=-2.0 .. 282.7
Cancel bounds: x=-15.7 .. 48.6
Reload bounds: x=56.6 .. 223.0
fits=false
```

같은 harness의 400/320pt 27과 280pt 26/29는 fit했다. 280pt 영어 27에서 취소 버튼이 **일부** 잘리며, Esc와 보이는 일부 버튼까지 불가능하다고 주장하지 않는다. 기존 좁은 화면 테스트는 한국어 26/29에 집중해 이 조합을 놓쳤다.

**수정 방향:** 번역된 버튼의 폭 합계를 측정해 공용 footer에서 줄바꿈/세로 배치한다. shell/body의 하단 높이 예약도 실제 footer 높이를 반영한다. 긴 번역의 모든 동작 버튼 bounds를 viewport 안에서 검사해야 한다.

## 성능·메모리 및 제외한 후보

- 검토 범위에서 확정 가능한 메모리 누수/불필요한 전체 문서 복제는 추가로 발견하지 않았다. 사례별 Area ID 수 제한, inline TextEdit 상태 해제, 문서 이름·길이 전달, 비동기 작업 경로를 검토했다. 장시간 live RSS/힙 프로파일을 실행한 결론은 아니다. F1 해결 시 Area를 문서마다 무한히 늘리는 방식은 피한다.
- 폴더 이동 CAS `Ok(Stale)`이 일치하는 안내를 정리하는 것은 이전 제안이 더 이상 유효하지 않음을 나타낸다. `Err` 재시도와 다른 상태라 버그로 보고하지 않았다.
- 새 파일 생성의 합성 Enter→IME Commit 순서 후보는 실제 macOS vendored winit의 `interpretKeyEvents`/Commit/Return 억제 흐름에서 같은 근거를 확인하지 못했다. 실제 발생이 검증되지 않아 확정 결함에 넣지 않았다.

## 실제 실행한 검증

| 실행 주체/명령 | 결과 |
| --- | --- |
| Root/App `cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo popup -- --test-threads=1` | 30 passed, 0 failed, 8 ignored |
| Logic reviewer/App `confirmation` | 9 passed, 0 failed, 4 ignored |
| Logic reviewer/App `document_popups` | 10 passed, 0 failed, 1 ignored |
| Quality reviewer/App `popup_audit`; Connector `popup_audit` | 10 passed; 1 passed |
| 독립 CLI/full App `cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=1` | 2,489 passed, 0 failed, 28 ignored, 46.96s |
| Root/CLI `cargo test --offline --locked -q -p connector-ui` | 17 passed |
| CLI `cargo test --offline --locked -q -p i18n` | 8 passed |
| CLI/storage `workspace_find_or_create` | 10 passed |
| Root/storage `workspace_find_or_create_refreshes_remounted_device_at_same_path` | 1 passed |
| CLI `cargo clippy --offline --locked -q -p deppy-sijo -p connector-ui --all-targets -- -D warnings` | exit 0 |
| CLI `cargo fmt --all -- --check`; `git diff --check` | exit 0 |
| Root/actual dirty popup two-document focus probe | exit 0; 다음 문서의 의도하지 않은 Discard 재현 |
| Root/actual WorkspaceUi → conflict() first-frame probe | exit 0; 최초 `race\r` WriteInput, 이후 차단 재현 |
| CLI/root search/menu probes and CLI old-predicate comparison | exit 0; 현재 회귀/이전 정상 동작 비교 |
| Root actual popup geometry probe | exit 0; 280pt en-US27 overflow 측정 |

별도 재현 하네스는 `/tmp`에만 두었다. 제품 테스트 통과는 위 5건이 수정됐다는 뜻이 아니다. live Deppy/OS picker/사용자 파일 삭제·저장·별도 볼륨 mount는 실행하지 않았다. F4의 허용 동작은 실제 저장 API 테스트로 확인했고 별도 볼륨 충돌 시나리오는 코드 분석이다.

## 재현 파일과 다음 수정 순서

- CLI 원본 로그: `/tmp/deppy-popup-full-codex-review-20260930.log`.
- F1: `/private/tmp/deppy-popup-security.1RnWvU/main.rs` 및 `repro`.
- F2: `/private/tmp/deppy-popup-security-cargo-20260930/crates/app/src/ui/workspace.rs`, `probe.log`. 실제 제품 모듈을 참조하는 별도 Cargo probe; main은 이 offscreen harness만 호출한다.
- F3: `/tmp/deppy-review-probe.FDwM2c/{search,menu,search-baseline,menu-baseline}`.
- F5: `/tmp/deppy-popup-review-probe-20260930/src/main.rs`, `layout.log`.

수정 작업에서 F1의 문서 전환 focus를 먼저 막고, F2/F3를 함께 입력 소유권 정책으로 정리한다. F4 backend 식별을 독립 변경하고 F5를 공용 footer에서 수정한다. 각각 actual-widget/frame 회귀 테스트를 추가해 기존 대규모 테스트의 빈틈을 보완한다. 수정 빌드를 전달하려면 0.4.10보다 높은 버전과 artifact 검증이 필요하며, 재실행에는 새 명시적 요청이 필요하다.

## 후속 수정

2026-10-01 사용자 수정 요청으로 위5건과 후속 검토에서 확인한 경계 오류를 수정했다. 과거 미해결 목록은 당시 상태 기록이다. 최신 결과는 [수정·재검증 보고서](2026-10-01-popup-review-fixes.md)를 따른다.
