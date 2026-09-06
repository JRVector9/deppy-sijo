# 터미널 파일 드롭 → 문서 탭 설계

상태: 2026-08-24 사용자 승인, 구현 전

선행 설계: `docs/superpowers/specs/2026-08-21-document-tab-design.md`

## 1. 목표

Finder 또는 Deppy 파일 트리에서 **로컬 터미널 본문**으로 파일을 드롭하면, 그
경로를 셸 입력으로 붙여넣지 않고 Deppy의 기존 문서 탭으로 연다.

확장자 allowlist를 두지 않는다. 이 기능에서 “서비스가 읽을 수 있는 파일”은 기존
`document_io::load_document`가 안전하게 받아들이는 파일로 정확히 정의한다.

- regular file
- 유효한 UTF-8
- 8 MiB 이하

1 MiB 이하는 기존 `Full` 티어로 편집·저장할 수 있고, 1 MiB 초과 8 MiB 이하는
기존 `ViewOnly` 티어로 읽기 전용으로 연다. 비 UTF-8, 디렉터리, 읽기 실패, 8 MiB
초과는 PTY 입력으로 폴백하지 않고 기존 문서 오류/거부 표면에서 이유를 보여준다.

## 2. 확인된 현재 상태와 실측

현재 확장자 제한은 `crates/app/src/ui/file_tree.rs`의
`classify_document_target`에만 있다. 이 함수는 `.md`, `.markdown`, `.txt`, `.log`,
확장자 없는 경로만 문서 액션으로 보내고 나머지는 OS 기본 앱으로 보낸다.

반면 `crates/app/src/document_io.rs`의 `load_document`는 확장자를 검사하지 않는다.
regular file 여부, 크기, UTF-8 여부만 판정한다. 따라서 `.rs`, `.json`, `.toml`,
`.yaml`, `.tsx`, `.html`, `.svg`도 위 조건만 만족하면 이미 같은 로더로 열 수 있다.

2026-08-24 실제 검증:

- `CARGO_BUILD_JOBS=2 cargo test -p deppy-sijo document_io::tests --locked -- --test-threads=1`
  - 13 passed, 0 failed, 3 ignored(수동 성능 벤치)
- Git 추적 regular file 919개를 바이트 단위 UTF-8/크기 조건으로 스캔
  - 로더 허용: 896개
  - 비 UTF-8: 21개(폰트, PNG, ZIP, 바이너리 등)
  - 8 MiB 초과: 2개
  - 허용 확장자에는 `.rs`, `.md`, `.json`, `.recording`, `.toml`, `.tsx`, `.html`,
    `.txt`, 확장자 없음, `.sh`, `.js`, `.svg`, `.yml`, `.css`, `.ts`, `.py`, `.yaml`
    등이 실제로 포함됐다.

첫 스캔에 사용한 macOS `iconv`는 일부 유효한 UTF-8 소스에도
`Inappropriate ioctl for device`를 반환해 79개를 잘못 거부했다. 이 결과는 폐기했고,
파일 바이트를 UTF-8 문자열로 직접 검증한 재측정만 위 근거로 사용한다.

## 3. 사용자 동작 계약

### 3.1 단일 파일

로컬 터미널 본문에 파일 하나를 드롭하면 다음 순서로 처리한다.

1. 정확히 포인터 아래의 local pane이 드롭을 소유한다.
2. Workspace leaf는 경로만 App에 intent로 올린다.
3. App은 기존 `open_document` → `begin_document_open` → bounded load worker 경로를 쓴다.
4. 로드 가능한 파일은 문서 그룹에 탭으로 추가되고 활성화된다.
5. 기존 동작과 같이 `PathBuf`가 정확히 같은 경로가 이미 열려 있으면 새 탭을 만들지
   않고 기존 탭을 활성화한다. 이번 범위에서 canonicalize나 별도 경로 정규화는 하지 않는다.
6. 로드 불가 파일은 PTY에 경로를 쓰지 않고 문서 탭의 기존 오류/거부 상태를 표시한다.

### 3.2 여러 파일

한 번의 Finder 드롭에 경로가 여러 개 있으면 원래 `dropped_files` 순서를 보존해 모두
App으로 올린다. App은 그 순서대로 문서 열기를 요청하며 **마지막 경로의 탭이 활성**이다.

기존 안전 상한은 바꾸지 않는다.

- `DOCUMENT_TABS_MAX = 8`
- `DOCUMENT_TOTAL_RETAINED_BYTES_MAX = 24 MiB`

상한에 닿으면 기존 `plan_document_eviction` 규칙대로 오래된 clean 비활성 문서를
정리한다. dirty/active 문서 때문에 자리를 만들 수 없으면 기존 cap 안내를 보이고,
경로를 PTY로 보내지 않는다. 드롭 파일 수를 이유로 새 무제한 보관 구조를 만들지 않는다.

자체 검토에서 기존 바이트 상한의 적용 시점에 빈틈이 확인됐다. 새 탭은 `Loading` 상태의
빈 `source`/`saved_source`로 먼저 추가되므로 여러 파일을 빠르게 열면 pre-open
`plan_document_eviction`은 각 탭을 0 byte로 센다. 로드 결과가 뒤늦게 각 탭에 적용될 때는
24 MiB 상한을 다시 검사하지 않아, 8개 탭이 모두 8 MiB급이면 두 사본 기준 최대 약
128 MiB를 다음 문서 열기 전까지 보유할 수 있다. 무한 누수는 아니지만 승인된 24 MiB
계약과 다르며 다중 드롭이 이 경로를 직접 만든다.

따라서 이번 구현은 로드 결과의 `source`를 문서에 복제하기 **직전**에도 prospective
retained bytes를 계산한다. 24 MiB를 넘으면 오래된 clean 비활성 문서를 먼저 닫고,
그래도 자리를 못 만들면 결과 문자열을 버리고 새 탭을 닫은 뒤 기존 cap 안내를 표시한다.
기존 dirty/active 문서는 자동으로 닫지 않는다. 이때 닫는 새 탭은 아직 내용을 적용하지
않은 `Loading` placeholder라 사용자 편집을 잃지 않는다. 결과 문자열 하나를 받는 순간의
유계 임시 메모리는 허용하되, App 상태에 보유된 `source + saved_source` 합은 결과 적용 후
24 MiB 이하가 되게 한다.

### 3.3 드롭 종류별 동작

| 입력 | 터미널 본문 위 | 그 외 영역 |
|---|---|---|
| Finder 파일 경로 | 문서 탭 열기 | 기존 영역별 동작 유지 |
| 파일 트리 `PathBuf` | 문서 탭 열기 | 기존 파일 트리 DnD 유지 |
| `TerminalTextDragPayload` | 기존처럼 터미널 텍스트 붙여넣기 | 기존 동작 유지 |
| 디렉터리/바이너리/대용량 경로 | 문서 오류/거부 표시, PTY 쓰기 없음 | 기존 영역별 동작 유지 |

파일 트리 행의 더블클릭 확장자 정책과 컨텍스트 메뉴의 OS 열기는 이번 범위에서 바꾸지
않는다. attached cross-workspace pane도 기존처럼 로컬 파일 DnD 대상이 아니다.

## 4. 선택한 아키텍처

### 4.1 권장안: Workspace hit-test, App 문서 소유

데이터 흐름:

```text
Finder / file-tree path drop
  → WorkspaceUi: 정확한 local pane hit-test + 경로 intent 생성
  → WorkspaceSurfaceOutput
  → App: open_document(path) 반복
  → 기존 bounded document load worker
  → 기존 OpenDocument 탭 상태
```

`WorkspaceUi`는 파일을 열거나 내용을 읽지 않는다. 기존처럼 pane rect, 입력 소유권,
typed DnD payload만 판정한다. `WorkspaceSurfaceOutput`과 내부 `PaneRenderOutput`에는
실제 드롭 프레임에만 채워지는 `Vec<PathBuf>` 형태의 일회성 intent를 추가한다.
`PaneRenderOutput::merge`는 경로 순서를 유지해 append한다.

App은 CentralPanel 렌더가 끝날 때까지 경로 intent를 짧게 모은 뒤, 렌더 borrow가 끝난
지점에서 각 경로를 기존 `open_document`로 전달한다. 새로운 문서 상태 기계, 새 I/O
스레드, 파일 내용 복사본을 만들지 않는다. 다만 §3.2에서 확인된 기존 바이트 상한 빈틈은
App의 load-outcome 적용 경계에서 순수 admission helper를 추가해 닫는다.

### 4.2 채택하지 않은 안

**App 전역 raw drop intercept**: 문서 소유는 중앙화되지만, App은 terminal renderer와
pane background가 계산한 실제 표면 rect/입력 소유권을 다시 복제해야 한다. 분할 pane,
사이드바 파일 반입, 컴포저 드롭과 한 이벤트를 함께 소비할 위험이 있어 채택하지 않는다.

**Workspace에서 동기 preflight/load**: 드롭 순간 regular/UTF-8/크기를 곧바로 알 수
있지만 렌더 경로에서 파일 I/O가 발생한다. 기존 bounded loader와 정책이 중복되고 큰
파일/느린 볼륨에서 프레임을 막으므로 채택하지 않는다.

## 5. 세부 라우팅

### 5.1 OS 드롭

현재 `render_pane`은 `raw.hovered_files`/`raw.dropped_files`와
`os_drag_pointer_pos` 폴백으로 `os_over_pane`을 계산한다. 이 정확한 hit-test는 유지한다.

현재의 `paths_insert_paste_bytes` + `RuntimeCommand::WriteInput` 분기만 문서-open intent로
교체한다. 같은 raw drop을 여러 split pane이 볼 수 있어도 `pane_rect.contains(pos)`가
참인 pane 하나만 경로를 올린다. pane 헤더처럼 본문 rect 밖의 드롭은 계속 무시한다.

### 5.2 파일 트리 typed 경로 드롭

현재 pane background와 terminal renderer response에는 각각 overlapping drop target이
있다. 둘 중 실제 topmost response가 `PathBuf` payload를 한 번만 release한다는 기존
계약을 유지하되, release 성공 시 `WriteInput` 대신 같은 문서-open intent를 올린다.

`TerminalTextDragPayload`는 타입이 다르므로 그대로 `WriteInput`을 사용한다. 타입 확인
후 payload를 take하는 기존 unrelated-payload 보호도 유지한다.

### 5.3 App 적용

primary workspace의 `WorkspaceSurfaceOutput`에서만 경로를 받는다. App은 다음 규칙으로
순서대로 `open_document`를 호출한다.

- 경로 순서 보존
- 같은 경로면 기존 탭 활성화
- 새 경로면 기존 멀티 문서 탭 로직 사용
- 마지막으로 admission된 성공/오류 탭이 최종 활성. cap 때문에 새 `Loading` placeholder가
  거부되면 기존 인접 탭으로 복귀
- cap 거부가 발생해도 이후 경로를 위한 기존 bounded 판정은 계속 적용하되, PTY 폴백 없음
- 로드 결과 적용 전 prospective `source + saved_source` 보유량을 검사하고 필요하면 오래된
  clean 비활성 탭을 닫음

드롭은 session이 존재하고 local input이 활성인 터미널 본문에서만 시작하므로, 보통
`open_document`는 즉시 `begin_document_open`으로 간다. pane이 없는 경우를 위한 기존
`pending_document_open` 단일 슬롯을 이번 기능에서 새 batch queue로 확장하지 않는다.

## 6. 오류와 사용자 피드백

새 오류 체계를 만들지 않는다. 기존 `DocumentLoadOutcome`과 문서 탭 표시를 재사용한다.

- `Loaded`: 편집 가능한 Source 탭
- `ViewOnly`: 읽기 전용 탭 + 기존 크기 안내
- `Refused`: 8 MiB 초과 안내
- `Binary`: UTF-8이 아니라는 안내
- `Failed`: not found, invalid file type, permission/read/metadata/change 오류 안내

비 UTF-8을 임의 lossy 변환하지 않고, 바이너리를 텍스트처럼 보이게 하지 않는다. 경로와
문서 내용은 기존 정책대로 로그에 남기지 않는다.

## 7. 성능·메모리·repaint 계약

- 파일 내용은 기존 단일 bounded worker에서만 읽는다.
- 한 파일당 최대 읽기 프로브는 기존 8 MiB + 1 byte를 넘지 않는다.
- 문서 상태는 기존 8 tabs/24 MiB retained cap을 그대로 쓰며, 다중 in-flight load 결과를
  적용한 직후에도 24 MiB 이하임을 보장한다.
- 출력의 `Vec<PathBuf>`는 실제 드롭 프레임에만 경로를 소유하고 다음 프레임에 유지하지
  않는다. 새 캐시·전역 큐·타이머·스레드를 추가하지 않는다.
- hover 피드백은 기존 입력 이벤트 기반 repaint를 쓴다. periodic repaint나 idle polling을
  추가하지 않는다.
- terminal snapshot, PTY 크기, split transaction, resize presentation fence는 건드리지
  않는다. 문서 열기가 terminal repaint/resize 명령을 유발해서는 안 된다.

## 8. 테스트 설계

TDD에서 먼저 아래 RED를 만든다.

### 8.1 순수/leaf 테스트

- `.rs`, `.json`, `.toml`, `.yaml` 경로가 확장자 때문에 거부되지 않고 문서 intent로 간다.
- 내부 `PathBuf` drop은 `WriteInput`을 만들지 않고 정확히 한 경로 intent를 만든다.
- `TerminalTextDragPayload` drop은 계속 `WriteInput`을 만든다.
- OS 다중 drop은 원래 순서의 모든 경로를 intent로 만든다.
- pane 헤더/밖 OS drop은 intent와 `WriteInput` 모두 만들지 않는다.
- split 두 pane 중 포인터 아래 pane 하나만 drop을 올린다.
- unrelated typed payload를 앞선 handler가 소비하지 않는다.

### 8.2 App 라우팅 테스트

- 한 frame의 여러 경로를 순서대로 `open_document`에 적용한다.
- 모든 경로가 cap admission에 성공한 경우 마지막 경로가 active document가 된다.
- 이미 열린 경로는 중복 생성 없이 기존 id가 활성화된다.
- drop routing에는 `paths_insert_paste_bytes`/`RuntimeCommand::WriteInput` 폴백이 없다.
- 8-tab/24-MiB cap과 dirty 보호는 기존 eviction 규칙을 그대로 사용한다.
- 여러 큰 파일 결과가 순서대로 도착해도 load-outcome 적용 뒤 retained bytes가 24 MiB를
  넘지 않는다.
- prospective load를 위해 clean 비활성 탭은 정리할 수 있지만 기존 dirty/active 탭은
  자동으로 닫지 않는다. 자리를 못 만들면 새 `Loading` placeholder만 닫고 결과 내용을
  보유하지 않은 채 cap 안내를 표시한다.

### 8.3 기존 로더 회귀

- extension-agnostic regular UTF-8 fixture가 `Loaded`가 된다.
- 1 MiB/8 MiB 경계, non-UTF-8, missing/non-regular 결과가 기존과 같다.
- 문서 경로/내용 로그 금지 테스트가 계속 통과한다.

### 8.4 검증 게이트

- focused Workspace drop tests
- Workspace 전체 테스트 그룹
- focused App document/drop routing tests
- `document_io::tests`
- `cargo test -p deppy-sijo --locked -- --test-threads=1`
- strict Clippy with `-D warnings`
- `cargo fmt --all --check`
- `git diff --check`
- 코드 리뷰 후 지적 반영과 관련 회귀 재실행
- signed macOS rebuild, deep/strict codesign verification, bounded relaunch

## 9. 수동 인수 기준

1. Finder에서 `.md`, `.rs`, `.json`, `.toml`, `.yaml`을 각각 터미널 본문에 드롭하면
   문서 탭이 열리고 셸 prompt에는 경로가 입력되지 않는다.
2. 여러 텍스트 파일을 한 번에 드롭하면 각각 탭이 생기고 마지막 파일이 보인다.
3. 이미 열린 파일을 다시 드롭하면 탭이 중복되지 않는다.
4. PNG나 폰트를 드롭하면 바이너리 안내가 보이고 셸 입력은 바뀌지 않는다.
5. 1 MiB 초과 8 MiB 이하 UTF-8은 읽기 전용으로 열린다.
6. 8 MiB 초과 파일과 디렉터리는 이유가 표시되고 셸 입력은 바뀌지 않는다.
7. 분할된 pane에서는 포인터 아래 pane에 연결된 문서 그룹만 활성화된다.
8. 터미널 텍스트 drag는 기존처럼 붙여넣어진다.
9. 드롭이 끝난 뒤 idle CPU/repaint가 증가하지 않고 terminal 화면이 깜빡이지 않는다.
10. 여러 ViewOnly급 파일을 한꺼번에 드롭해도 clean 비활성 탭만 정리되며 dirty 문서는
    보존되고, 문서 retained bytes가 24 MiB를 넘지 않는다.

## 10. 범위 밖

- 파일 트리 더블클릭의 기존 확장자 allowlist 변경
- 바이너리/hex viewer
- 비 UTF-8 인코딩 자동 감지 또는 lossy 변환
- 8 MiB 상한, 8-tab/24-MiB retained cap 확대
- remote/SSH 파일 다운로드와 attached workspace pane 로컬 파일 드롭
- 최근 드롭 파일 목록 영속화
- terminal split/resize presentation fence의 별도 rapid-repeat 문제 수정
