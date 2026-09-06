# Grok 사용량 수명주기와 파일명 Unicode 교정 설계

## 목적

이미 배포된 Grok 하단 사용량 표시와 폴더 트리 한글 표시에서 리뷰로 확인된 네 가지 결함을 최소 범위로 교정한다. 정상 Grok 값의 표시 형식, 일반 터미널 IME/UTF-8 처리, raw 파일 경로의 실제 신원, 기존 파일 작업 정책은 바꾸지 않는다.

## 확인된 원인

1. Grok worker는 `Option<GrokUsage>`만 채널로 보내고 UI가 채널을 읽는 순간을 측정 시각으로 기록한다. Grok을 비활성화한 동안 완료된 결과가 채널에 오래 머물면 재활성화 시 오래된 결과가 새 값으로 둔갑한다.
2. Grok 1.0.5가 내는 최종 상태 중 `No billing data available`, `Usage limits are managed by your team`, `Couldn't load usage`가 완료 판정 목록에 없다. 값이 더 도착하지 않아도 worker가 25초 deadline까지 남는다.
3. 파일 트리 host listing은 `OsString` 이름을 `to_string_lossy()`로 영구 변환한 뒤 그 문자열로 `PathBuf`를 재구성한다. invalid UTF-8 이름은 실제로 존재하지 않는 U+FFFD 경로로 변한다.
4. `b072cff`는 트리 행만 NFC로 표시한다. 문서 탭, 이름 변경 입력, 삭제/dirty 확인, 새 파일 위치, 터미널의 파일 열기 메뉴는 raw macOS NFD 문자열을 그대로 표시한다.

## 접근안 비교

### A. typed identity와 display projection 분리 — 채택

- Grok 채널 payload에 worker 완료 `Instant`를 포함하고 UI는 그 시각을 그대로 보존한다.
- 설치본에서 확인한 최종 no-data 문구만 완료 허용목록에 추가한다.
- listing/tree는 raw `OsString`을 신원으로 보존하고 NFC lossy 문자열은 렌더링·정렬용 projection으로만 둔다.
- 공통 UI helper가 path/component를 NFC 표시 문자열로 바꾸며 모든 확인된 시각 표면이 이를 사용한다.

장점은 실제 경로 신원과 화면 표현이 분리되고 기존 보안/자원 상한을 유지한다는 점이다. 변경 파일은 현재 결함이 있는 경계로 제한된다.

### B. 모든 경로를 NFC로 정규화 — 제외

코드는 단순하지만 APFS 외 파일시스템, invalid UTF-8, canonical-equivalent 이름을 실제 신원과 다르게 만들 수 있다. 드롭·열기·이름 변경 대상이 존재하지 않는 경로가 될 수 있어 제외한다.

### C. invalid UTF-8 항목을 숨기고 UI별로 개별 정규화 — 제외

파일을 임의로 숨기는 기능 손실이 생기고 동일한 정규화가 여러 화면에 다시 흩어진다. 이후 표면 추가 시 회귀가 반복되므로 제외한다.

## 데이터 흐름

### Grok

`worker fetch 완료 → Option<(completed_at, GrokUsage)> → pending receiver → UsageState.usage(completed_at, value) → STALE_AFTER 판정 → 하단 표시`

worker 실패는 기존처럼 `None`이며 마지막 정상값 정책을 바꾸지 않는다. UI를 오래 멈춘 뒤 받은 성공값은 완료 시각 기준으로 즉시 stale 판정될 수 있다. no-data 문구는 panel이 최종 상태에 도달했다는 신호일 뿐이며 파서는 여전히 `None`을 반환한다.

### 파일명

`read_dir OsString → bounded FileTreeListingItem(raw name + NFC display name) → TreeNode(raw name + display) → FlatRow(raw PathBuf + display) → UI action은 raw PathBuf / UI label은 display helper`

메모리 상한은 raw `OsStr::as_encoded_bytes().len()`으로 계산한다. 정렬은 NFC display를 우선하고 raw 이름을 tie-break로 사용해 서로 다른 이름을 안정적으로 유지한다. 숨김 판정과 node lookup은 raw 이름을 사용한다.

## UI 표시 정책

- 트리 행, 문서 탭, 문서 dirty 확인, rename 입력, 영구삭제 확인, 새 파일/폴더 위치, 터미널 파일 열기 메뉴는 NFC 표시 문자열을 사용한다.
- 클립보드에 복사하는 경로, PTY로 삽입하는 경로, DnD payload, 파일 작업 request는 정규화하지 않는다.
- rename 입력의 NFC 표시값이 source raw 이름과 canonical-equivalent이면 no-op으로 처리해 표시만 바꾼 사용자가 실제 경로 rename을 일으키지 않게 한다.

## 오류·자원 정책

- 기존 item/path/byte cap, NUL 거부, single-flight, 60초 refresh, 10분 stale, 25초 hard deadline을 유지한다.
- Grok의 알려지지 않은 출력은 기존 fail-closed timeout을 유지한다. 일반적인 `usage`/`error` 부분 문자열로 조기 종료하지 않는다.
- invalid UTF-8 파일명은 표시에서 U+FFFD가 보일 수 있지만 action identity는 raw bytes이므로 올바른 파일을 가리킨다.

## 테스트 계약

1. 600초보다 오래 전에 완료된 buffered Grok 결과는 수신 직후 stale이다.
2. 설치본의 세 no-data 문구 모두 panel 완료로 판정된다.
3. Unix invalid UTF-8 filename을 host listing과 tree flat row가 byte-for-byte 보존한다.
4. 서로 다른 raw 이름이 같은 lossy 문자열이 되어도 row identity가 충돌하지 않는다.
5. NFD 한글 component/path는 모든 공통 표시 helper에서 NFC로 보인다.
6. canonical-equivalent rename 표시값은 raw source를 그대로 둔 no-op이다.
7. 기존 Grok, file-tree, document, workspace, terminal Korean/IME, i18n, boundary, strict Clippy 테스트가 회귀 없이 통과한다.

## 전달

두 독립 구현을 병렬 개발한 뒤 root가 통합 diff와 교차 경계를 리뷰한다. Workstep에 따라 직접 Codex 리뷰를 반영하고 한국어 Conventional Commit, Obsidian 프로젝트 일지, Developer-ID 서명 패키징, deep/strict codesign 검증, 이전 exact bundle 종료, 새 exact bundle 재실행까지 완료한다.
