# 전체 안정성·성능·효율 코드 리뷰 (2026-07-10)

## 결론

- 판정: **코드 차단 해제 / 출시 실측 대기**. Critical 또는 현재 테스트로 재현된 production
  crash는 없고, 이 문서에서 발견한 High 3건과 Medium 2건은 모두 반영했다.
- 파일 트리, runtime 이벤트, live warm workspace에 프로세스 수준 hard cap을 추가했다.
- 환경/API 화면의 집계·filesystem·keyring·dotenv I/O는 bounded background worker와
  generation 검증으로 옮겼고, `.env` 저장은 crash-safe atomic replace로 바꿨다.
- 전체 workspace 테스트, Clippy `-D warnings`, security-scan, perf-smoke, i18n-check가 통과했다.
- GUI release build의 실제 RSS/CPU/frame-p95/100k-file soak는 아직 측정되지 않았다.

## 이번에 수정한 사항

1. 환경/API 상세 헤더가 프로젝트 경로에 `Path::is_dir()`를 매 프레임 호출하던 병목을
   제거했다. 프로젝트 목록 캐시의 `path_missing` 결과를 재사용한다.
2. 외부 dotenv 동기화로 삭제된 키의 마스킹 상태가 `HashSet`에 계속 남아 같은
   workspace에서 무제한 증가할 수 있던 상태 누적을 제거했다.
3. `.env` 읽기 오류(권한, 잘못된 UTF-8, 일시적 I/O)를 빈 파일로 오인해 전체 파일을
   덮어쓰는 데이터 손실 경로를 차단했다. NotFound만 새 파일로 취급한다.
4. `.env` 중복 키 편집은 마지막 실효 정의를 갱신하고 중복을 제거하며, 삭제는 모든
   중복 정의를 제거하도록 고쳤다.
5. `.env.local` 읽기 실패를 조용히 무시한 부분 동기화가 기존 DB/keyring 항목을
   삭제된 키로 오판하지 않도록, 존재하는 dotenv 파일의 읽기 오류를 즉시 전파한다.
6. 중복돼 있던 `#[test]`를 정리하고 실제로 실행되지 않던 dotenv 파서 테스트를 복구했다.

관련 회귀 테스트는 `crates/app/src/dotenv_sync.rs`에 추가했고 현재 11개 모두 통과했다.

## 후속 반영 사항

### High 1 — 파일 트리 listing/watcher 무제한 구조: 반영 완료

- listing은 프로세스 전역 4개 worker pool로 통합했다. 사이드바를 반복 생성하거나 루트를
  빠르게 바꿔도 listing thread 수는 늘지 않는다.
- listing job 64개, 결과 16개, watcher 이벤트 512개, 파일 조작 결과 32개로 채널을 제한했다.
  파일 조작 동시 thread도 4개로 제한했다.
- watcher는 프레임당 256개만 소비하며 overflow는 개별 이벤트를 버퍼링하지 않고 root refresh
  한 건으로 축약한다. listing job overflow도 동일하게 deferred root refresh로 복구한다.
- macOS watcher Drop이 사라진 감시 루트에서 60초 이상 UI를 막을 수 있는 경로는 전역 slot
  4개 + 고정 reaper로 옮겼다. 테스트 빌드는 OS watcher 대신 채널 주입 로직만 검증해 파일
  트리 테스트 31개가 기존 60초 초과/전체 118초대에서 0.11초로 단축됐다.

### High 2 — runtime 일반 이벤트 무제한 채널: 반영 완료

- local/remote 구독자의 durable event queue를 구독자당 1,024개로 제한하고, UI drain을
  프레임당 256개로 제한했다. Viewport/input pressure/resource usage는 최신값 slot을 유지한다.
- durable backlog를 나누는 중에도 Spawn/Mux → Viewport 순서가 뒤집히지 않도록 최신값 slot을
  잠시 되돌리는 happens-before 처리를 추가했다.
- queue overflow는 atomic degraded 신호로 surface하고 느린 구독자를 끊는다. 앱은 이미 큐에
  들어온 이벤트를 budget 단위로 끝까지 처리한 뒤 자동 재구독하고, active workspace에는 전체
  mux/viewport snapshot을 다시 요청하며 사용자에게 경고를 표시한다.
- warm/숨김 replay의 Viewport, status, resource, input pressure도 최신값으로 coalesce해 별도
  `pending_events` 누적 경로를 세션 수 기준으로 제한했다.

### High 3 — live warm workspace 무제한: 반영 완료

- idle warm LRU 2개 정책은 유지하고, live warm workspace에는 별도 hard cap 4개를 추가했다.
- 전환 후 예상 live warm 수를 먼저 계산해 cap 초과 전환은 runtime/PTY를 만들기 전에 거부한다.
  실행 중 작업을 강제 종료하지 않으며 대상과 cap을 설명하는 다국어 모달을 표시한다.

### Medium 1 — 환경/API 동기 I/O와 N+1 조회: 반영 완료

- workspace별 env/key 수는 `env_api_project_counts()` 단일 aggregate query로 가져온다.
- 프로젝트 집계와 경로 `is_dir`, secret reveal, dotenv stat/read/SQLite/keyring sync를 각각
  bounded background worker로 옮겼다. 결과는 workspace/root generation이 일치할 때만 적용한다.
- dotenv watcher/poll burst는 실행 중 1건 + 최신 deferred 1건으로 coalesce한다. 새 runtime은
  background dotenv 결과를 먼저 적용한 뒤 복원하며, 외장 볼륨/keyring 정지 시 5초 후 빈 env로
  안전하게 복원하고 늦은 결과는 이후 새 셸에 적용한다.
- UI에는 최초 로딩 spinner와 집계/secret 조회 실패 상태를 표시한다. 설정을 닫으면 평문 cache와
  stale worker 결과를 폐기한다.

### Medium 2 — `.env` non-atomic 저장: 반영 완료

- 같은 디렉터리에 고유 temp 파일을 0600으로 생성하고 기존 권한을 보존한 뒤 `write_all`,
  `sync_all`, atomic replace, Unix directory sync 순서로 저장한다.
- Unix는 `rename`, Windows는 `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`를 사용한다.
- 교체 실패 시 원본을 보존하고 temp를 제거하는 테스트와 권한 보존 회귀 테스트를 추가했다.

## 최초 발견 근거(반영 전 기록)

### 반영 전 High 1 — 파일 트리 listing/watcher가 무제한 스레드·채널 구조

근거:

- `FileTreeUi::new`는 operation/listing 결과에 `std::sync::mpsc::channel()`을 사용한다.
- watcher도 무제한 채널을 사용하고, UI는 watcher burst를 `while try_recv()`로 한 프레임에
  끝까지 비운다.
- 모든 listing 요청이 `std::thread::spawn`으로 새 OS thread를 만든다. 동시 listing 수와
  결과 채널 길이에 hard cap이 없다.
- 결과는 2,048개 단위 chunk이고 정상 결과는 프레임당 4개만 적용한다. epoch/token 취소는
  stale 작업을 줄이지만, 새 요청 수·동시 thread·현재 epoch backlog 자체는 제한하지 않는다.
- 이번 전체 테스트에서 앱 테스트가 118.39초 걸렸고 파일 트리 테스트 7개가 60초 초과
  경고를 냈다. 이 수치만으로 production 병목을 증명하지는 않지만, 집중 부하 측정이 필요한
  영역이라는 기존 감사 결과와 일치한다.

영향:

- 큰 트리를 빠르게 펼치거나 watcher 이벤트가 폭주하면 thread 수, heap, 결과 backlog가
  증가하고 UI가 stale 결과를 따라잡느라 멈출 수 있다.

권고:

- 고정 크기 listing worker pool, bounded result/watch channel, 최대 in-flight listing 수,
  overflow 시 root/dirty-dir 단일 refresh로 축약하는 정책을 도입한다.
- 100k direct-child, 빠른 expand/collapse/root-switch, watcher burst를 결합한 soak에서 최대
  thread 수·queue 길이·RSS를 assertion으로 남긴다.

### 반영 전 High 2 — runtime 일반 이벤트 구독 채널이 무제한

근거:

- Viewport, input pressure, resource usage는 최신값 slot으로 개선됐지만 그 외 lifecycle/status
  이벤트는 여전히 `channel()`과 `Sender::send`를 사용한다.
- `RuntimeEventReceiver::drain()`은 일반 이벤트 채널을 `try_iter().collect()`로 한 번에
  `Vec`으로 만든다. 느리거나 중단된 구독자에서 반복 status 이벤트가 쌓이면 backlog와
  일시 메모리 peak를 제한할 수 없다.

영향:

- 정상 앱 구독자는 wake 후 drain하므로 일상 재현 가능성은 낮다. 그러나 API가 허용하는
  slow secondary subscriber 또는 UI stall에서는 메모리 증가와 긴 프레임이 가능하다.

권고:

- durable 이벤트용 bounded queue와 coalescible 상태용 per-session slot을 분리한다.
- overflow 시 명시적 degraded/disconnect 이벤트를 내고, 프레임당 durable drain budget을 둔다.

### 반영 전 High 3 — live warm workspace는 수량 상한과 자동 suspend가 없음

근거:

- `MAX_WARM = 2`이지만 live session이 하나라도 있는 workspace는 상한 초과 시에도 evict하지
  않는다.
- 30분 자동 suspend도 live workspace에는 적용하지 않는다.

영향:

- 여러 workspace에 종료하지 않은 shell/agent를 남기고 계속 전환하면 runtime worker, PTY,
  terminal buffer, child process가 workspace 수만큼 계속 증가한다. 이는 의도한 작업 보호
  정책이지만 자원 상한 관점에서는 무제한이다.

권고:

- 작업을 강제 종료하지 않더라도 soft cap 초과 경고와 workspace별 RSS/child count를 노출한다.
- 새 workspace 활성화 전에 suspend/keep 선택을 받거나, 사용자가 명시적으로 pin한 작업만
  hard-cap 예외로 둔다.

### 반영 전 Medium 1 — 환경/API 화면의 동기 DB·filesystem·keyring 작업

근거:

- 프로젝트 카운트는 1초 TTL마다 workspace → profile → env var 형태의 N+1 SQLite 조회와
  각 path의 `is_dir()`를 UI thread에서 수행한다.
- 환경 표 cache miss 시 secret credential을 전부 keyring에서 동기 resolve한다.
- dotenv watcher/manual resync도 파일 읽기, SQLite 갱신, keyring 갱신을 UI thread에서 한다.

영향:

- workspace/profile/secret 수가 많거나 네트워크·외장 볼륨 및 keychain 응답이 느리면 설정
  창이 순간 정지할 수 있다. 이번 수정으로 매 프레임 path 조회는 제거했지만 1Hz 집계와
  cache-miss keyring 작업은 남아 있다.

권고:

- 프로젝트별 env/key count를 한 aggregate query로 가져온다.
- path 상태와 dotenv sync/keyring resolve를 background worker에서 수행하고 epoch으로 stale
  결과를 폐기한다. UI에는 loading/error 상태를 표시한다.

### 반영 전 Medium 2 — `.env` 수정이 crash-safe atomic write가 아님

근거:

- 읽기 오류에 의한 clobber는 이번에 막았지만 최종 저장은 여전히 `std::fs::write`로 대상
  파일을 직접 truncate/write한다.

영향:

- 쓰기 도중 앱/OS crash 또는 디스크 오류가 발생하면 `.env`가 부분 파일이나 빈 파일로
  남을 수 있다.

권고:

- 같은 디렉터리의 고유 temp 파일에 쓰고 flush/sync, 원본 권한 보존, atomic replace를
  플랫폼별로 구현한다. 실패 시 원본과 temp 정리 정책을 테스트한다.

### 검증 공백 — release hardware 측정 미완료

`docs/performance/release-hardware-measurements.md`의 Scenario A–E RSS, CPU, frame p95,
queue/pressure 값이 전부 Pending이다. 자동 테스트 통과는 실제 GUI idle, 20 panes/10 sessions,
hidden high-output, 100k-file tree, remote slow consumer soak를 대체하지 않는다.

## 확인된 방어 구조

- local runtime command queue, PTY output/input, MCP stdout, storage writer, remote command/outbound
  queue는 bounded 또는 coalesced 정책이 있다.
- terminal exited backend와 hidden scrollback에는 개수/byte budget이 있다.
- MCP stderr와 JSON-RPC line/payload에는 크기 상한이 있다.
- PTY 종료는 SIGHUP 무시 자손까지 정리하는 테스트가 통과했다.
- secret-like env/agent/MCP 값의 평문 DB 저장 차단, audit redaction, 기본 encrypted blob off
  보안 테스트가 통과했다.

## 실행한 검증

- `cargo test -p deppy-sijo dotenv_sync -- --nocapture`: 11 passed
- `cargo test -p deppy-sijo ui::file_tree::tests:: -- --nocapture`: 31 passed, 0.11s
- `cargo test --workspace`: 전체 통과. app 139 passed, 2 ignored(manual smoke), runtime 101,
  storage 45; 다른 crate와 doc-test 모두 통과
- `cargo clippy --workspace --all-targets -- -D warnings`: 통과
- `cargo run -p xtask -- security-scan`: 통과
  - boundary guard 통과; 명시적 UI DB/MCP/audit 예외 29개 유지
  - 19 crate 의존성 그래프 금지 edge/순환 없음
  - storage/MCP/audit/proxy 민감정보·정책 테스트 통과
- `cargo run -p xtask -- perf-smoke`: 통과
- `cargo run -p xtask -- i18n-check`: 통과

## 출시 전 우선순위

코드 우선순위 1–5는 반영 완료했다. 남은 출시는 다음 실제 장비 검증뿐이다.

1. release Scenario A–E RSS/CPU/frame-p95/queue 실측
2. 100k-file GUI expand/refresh 및 watcher burst 장시간 soak
3. remote slow-consumer 장시간 soak
