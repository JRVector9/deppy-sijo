| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | codex_app_server.rs:721/3039 | R01 막힌 stdin 쓰기를 종료가 취소하지 못함 | UI 종료·클라이언트 교체 대기 정체 | 취소 가능한 쓰기와 bounded 종료 |
| high | document_io.rs:204; app.rs:8499 | R02 비정규 파일을 blocking read/open | 문서 lane 또는 UI 전체 정지 | 비차단 open·동일 handle 종류 검증 |
| high | document_io.rs:422/342 | R03 저장 중 외부 편집을 덮어씀 | 외부 편집 손실·Saved 오표시 | commit 직전 identity/revision 재검증 |
| high | app.rs:21941; session_aux.rs:107 | R04 닫힌 세션의 dirty 문서 접근 불가 | 미저장 작업 복구 불가 | 닫기 전 보호·명시적 문서 복구 |
| high | storage-core/src/lib.rs:36 | R05 WAL 누락 백업에도 마이그레이션 성공 | 백업 복구 시 커밋 데이터 누락 | 일관된 SQLite backup·busy 검증 |
| medium | core/src/fs.rs:12 | R06 고정 임시 경로를 따라 기록 | 다른 파일 덮어쓰기·링크 교체 | 고유 temp를 배타적으로 생성 |
| medium | runtime/src/persistence.rs:248 | R07 이전 exit가 재실행 상태를 덮어씀 | DB 상태·복원 판단 오류 | 동일 UUID의 쓰기 순서·세대 보장 |
| medium | document_io.rs:355 | R08 링크 문서 저장이 링크를 교체 | 원래 대상 미저장·링크 소실 | 읽기·저장 대상 identity 일치 |
| medium | pty/src/lib.rs:1924 | R09 동결해도 foreground 작업 실행 | 동결 표시와 실제 작업 불일치 | 실제 작업 그룹 동결·결과 검증 |
| medium | runtime/src/agent_wrapper.rs:46 | R10 종료 후 늦은 터미널 응답이 셸로 전달 | 후속 셸에 입력 잡음 유입 | 에이전트 종료와 응답 소유권 fence |
| medium | ui/document.rs:276 | R11 편집 입력에 byte 상한 없음 | 저장 불가·메모리 성장·UI 지연 | 삽입 전 개별·전체 입력 budget |
| medium | connector-service/coordinator.rs:564 | R12 혼잡 시 import 완료 소유권 소실 | 가져오기 유실·재시도 거부 | enqueue 성공 후 소비·완료 보존 |
| medium | app.rs:5382; folder_identity.rs:51 | C01 non-Unix 폴더 설정이 항상 거부됨 | Windows 조건에서 경로 변경 불가 | 플랫폼별 유효 경로 검사 |
| medium | relay_client/tls.rs:201 | C02 receive 시한이 메시지 전체를 제한 못함 | Relay 활성화 시 종료 지연 | 전체 deadline·socket 취소 |
| low | agent_detect.rs:2328 | P01 process adjacency 반복 구축 | 큰 탐지 패스의 불필요한 CPU 비용 | snapshot당 한 번 생성 |

# 전체 코드·병목·오동작 병렬 리뷰 — 2026-10-14

현재 활성 제품 경로의 결함 **12건**, 플랫폼/활성화 조건이 있는 항목 **2건**, 측정한 최적화 후보 **1건**을 확인했다. 위 항목은 수정 완료 목록이 아니다. 제품 코드와 기존 테스트를 변경하지 않은 리뷰 결과다.

후속 사용자 요청에 따른 수정과 조건부 실측 결과는 [수정 검증 보고서](../qa/2026-10-14-full-review-fixes.md)에 기록했다. 이 문서는 수정 전 리뷰의 근거와 위치를 보존한다.

## 기준·분담·검증 범위

- Canonical: `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `d8826f0bdb8d89255f3448b5b61b6eee83faa87a`, source/test `0.12.1`. 직전 wide-cursor·popup 미커밋 수정도 포함한 현재 working tree를 검토했다. 오래된 nominal `deppy-sijo` checkout은 보존했다.
- GPT-6.1-Sol/xhigh 서브에이전트 3명: runtime/PTY/terminal/session/persist/core·agent 상태, UI/connector UI/파일·문서·Git/이력, 원격/PWA/Relay/MCP/auth/secret/storage. Root는 메인 App 프레임·워커 연결·종료·큐 처리, 후보 재현, 교차 대조와 중복 제거를 맡았다. Cargo는 Root 단일 슬롯만 사용했다.
- 412개 Rust/JS/TS/TOML/manifest 파일의 시작 SHA256와 최종 내용을 대조했다. 제품 diff도 시작 snapshot과 byte 단위로 동일하다. 기존 dirty/untracked 파일을 보존했다.
- inventory와 위험 중심 호출 경로를 검토했다. 모든 소스/테스트 행의 전수 독해, 모든 provider·플랫폼·실기기·GUI·잠자기 복귀를 실행 검증했다고 주장하지 않는다.
- 리뷰 스킬의 필수 gstack checklist가 없어서 해당 자동 워크플로는 중단했다. 직접 병렬 소스 리뷰와 systematic-debugging 절차로 검토했다. 자동 스킬 리뷰 완료로 기록하지 않는다.
- 실제 사용자 앱·세션·DB·키·clipboard를 변경하지 않았다. 앱 build/package/release/restart, commit/push를 수행하지 않았다. 별도 scratch의 tests-only 코드와 private temp 파일/DB/PTY/loopback 소켓만 재현에 사용했다.

## 실행 검증

이번에 실제 실행한 기존 검사:

```sh
CARGO_BUILD_JOBS=2 cargo test --offline --locked --workspace \
  --exclude deppy-sijo --exclude connector-ui --exclude terminal \
  --no-fail-fast -- --test-threads=1
```

**exit 0, 2,238 PASS / 0 FAIL / 18 기존 ignored**. 27 workspace package 중 나머지 24 package의 unit/integration/doc 검사다. subprocess의 중복 `1 passed; 67 filtered out` 결과는 합계에서 제외했다. 브라우저를 요구하는 ignored 검사는 실행한 것으로 세지 않았다.

직전 수정 단계의 App **3,181 PASS / 39 ignored**, connector-ui **35 PASS**, terminal **138 PASS / 4 ignored** 결과는 [기존 QA](../qa/2026-10-14-popup-context-folder-fixes.md)에 있다. 그 제품 소스는 이번 리뷰 시작·종료 사이 동일하다. 이 3개 전체 검사를 이번에 재실행했다고 주장하지 않는다. 기존 broad 검사 통과와 아래 추가 재현의 실패는 서로 모순되지 않는다. 기존 테스트가 해당 순서/경계 조건을 다루지 않았다.

추가 재현은 컴파일 성공 후 보호 기대 assertion이 **실제 실패(exit 101)**했다. 테스트 소스는 canonical에 넣지 않았으며 [근거 모음](evidence/2026-10-14-full-code-review/README.md)에 명령·소스·로그·SHA를 저장했다. 정적/컴포넌트 근거의 한계는 아래 각 항목에 명시했다.

## Details

### R01 — Codex App Server의 막힌 쓰기가 종료를 막음

[`write_message`](../../crates/app/src/codex_app_server.rs:3039)는 blocking `ChildStdin`에 JSON을 기록한다. [`shutdown`](../../crates/app/src/codex_app_server.rs:721)은 stop flag를 설정한 뒤 worker를 join한다. child를 종료하는 코드는 worker loop 이후에 있어, 쓰기가 막히면 거기에 도달하지 못한다. `ui/agent_sessions.rs:1561`의 명시적 종료와 `app.rs:33890`의 앱 종료 호출, 유휴 client 설정 교체(`ui/agent_sessions.rs:1690`)가 이 경로를 사용한다.

실제 production client를 fake local child로 시작했다. initialize/thread/start 완료 뒤 child가 stdin을 읽지 않는 조건에서 허용 상한 이내인 200KiB prompt를 보내면 shutdown은 500ms 안에 끝나지 않았다. fixture child만 종료하자 대기가 풀렸다. **0 PASS / 1 FAIL**, 실행 0.83s; `codex-shutdown.log`. 실제 외부 Codex server의 고장을 일으키거나 재현한 결과가 아니다. 쓰기의 cancellation/deadline과 child 종료→worker join 순서를 함께 고쳐야 한다.

### R02 — 일반 파일이 아닌 경로를 읽다가 lane/UI가 정지

[`read_document_bounded`](../../crates/app/src/document_io.rs:204)는 blocking `File::open` 이후에야 동일 handle의 `is_file()`을 검사한다. FIFO `.md`는 파일 트리의 확장자 분류(`file_tree.rs:455/3943`)와 목록 생성(`app.rs:15656`)을 통과한다. 단일 문서 load worker가 열린 FIFO에서 대기하면 정상 문서도 `Full`로 거부되고, worker Drop의 join도 완료되지 않는다.

실제 `document_io.rs`와 `lazy_worker.rs`를 그대로 include한 probe에서 250ms 후 결과 없음·정상 문서 admission `Full`을 확인했다. fixture writer로 해제한 뒤 `FileTypeInvalid` 및 정상 후속 문서 `Loaded`를 확인하고 cleanup한 후 assertion 실패했다. `document-fifo.log`.

같은 비정규 파일 처리 누락이 [`claude_usage_snapshot`](../../crates/app/src/app.rs:8499)에도 있다. metadata 길이만 검사하고 `read_to_string`한다. `app.rs:35627`은 Claude가 설치·활성화된 경우 이 함수를 **UI frame에서 직접 호출**한다. 2초 cache가 끝나면 FIFO의 길이 0이 통과해 UI가 대기한다. 실제 함수/신선도 helper를 byte 그대로 추출하고 `paths::home_dir`만 private temp 루트로 대체한 probe에서 250ms 동안 반환하지 않음을 확인했다. writer로 해제하면 정상 usage를 반환했다. `$HOME` 등 시스템 환경은 바꾸지 않았다. `usage-fifo.log`.

비차단 open→동일 handle의 regular-file 검증·bounded read가 필요하다. UI usage I/O는 worker/cache 경로로 옮겨야 한다. 사전 path metadata만 확인하는 방식은 교체 경쟁을 막지 못한다.

### R03 — 저장 도중 외부 편집의 손실

[`save_document`](../../crates/app/src/document_io.rs:422)가 revision을 한 번 검사한 뒤 temp의 write/sync/rename 동안 다시 확인하지 않는다(`:336–355`). 저장 요청 시작 후 외부 에디터가 원본을 수정해도 `Saved`로 덮어쓴다. 기존 테스트는 외부 변경을 저장 시작 전에만 넣는다.

실제 production save를 호출한 8MiB fixture에서 temp가 생긴 뒤 외부 write를 완료했고, 그 시점에 temp가 여전히 존재함을 확인했다. 결과는 `Saved`, 최종 파일은 Deppy의 8MiB 내용이며 `external_edit_lost=true`였다. **exit 101**, `document-save-race.log`. 8MiB는 실제 save API와 편집 성장 정책에서 허용되는 크기다. temp 작성 후 commit 직전 revision/identity를 재검증하고, 그 마지막 경쟁까지 어떤 보장을 제공할지 명확히 해야 한다. 내용은 항상 보존하고 충돌을 사용자에게 돌려줘야 한다.

### R04 — 세션을 닫으면 dirty 문서가 접근 불가능한 owner에 남음

`workspace.rs:8974/9041`은 문서 dirty 확인 없이 runtime `ClosePane`을 보낸다. App은 살아 있는 mux pane만 scope로 선택한다(`app.rs:5016/20615`). [`retain_live`](../../crates/app/src/session_aux.rs:107)는 문서가 있는 죽은 scope를 메모리에 남기지만 UI와 본문은 current scope만 표시한다(`app.rs:36350/21248`). 같은 파일을 다시 열어도 [`begin_document_open`](../../crates/app/src/app.rs:21941)의 current-only filter 때문에 기존 dirty buffer를 찾지 못한다. 문서 source의 영속화나 dead owner로 돌아가는 경로를 찾지 못했다.

실제 `session_aux.rs`, 생산 helper/filter와 UI type stub을 사용한 component probe: 닫힌 owner에서 새 live owner로 전환 후 visible IDs `[]`, same-path reopen `None`, retained dirty `true`. 강제로 dead scope를 선택해야만 원래 ID가 나타난다. **exit 101**, `document-orphan.log`. 실제 Workspace X/App GUI E2E는 수행하지 않았다. 소스 호출 연결과 생산 state component를 근거로 확정했다.

세션 독립성은 유지하면서 닫기 전 dirty 저장/취소, 또는 사용자가 명시적으로 복구할 수 있는 경로가 필요하다. 메모리에 보존하는 것만으로는 복구가 되지 않는다.

### R05 — WAL checkpoint의 busy를 무시해 백업이 커밋 데이터를 누락

[`open_with_migrations`](../../crates/storage-core/src/lib.rs:36)의 `PRAGMA wal_checkpoint(TRUNCATE)`는 row의 busy/log/checkpoint 결과를 읽지 않는다. busy여도 SQL 호출은 `Ok`라 main DB 파일만 `.bak`으로 복사하고 마이그레이션을 계속한다. live DB가 정상이고 backup의 integrity check가 정상이어도 최신 커밋이 backup에 없을 수 있다. GUI/proxy·background DB writer가 여러 연결을 사용하는 실제 구성에서 가능한 순서다.

실제 production storage-core 모듈 probe: reader transaction의 이전 snapshot을 유지한 동안 writer가 두 번째 row를 WAL에 커밋하고 v2 migration을 열었다. migration은 성공했지만 **live 2 rows / backup 1 row**, backup integrity `ok`. **exit 101**, 5.384s; `backup-production.log`. fixture DB만 사용했다. SQLite의 일관된 online backup 등으로 교체하거나 checkpoint 결과와 안정된 copy 조건을 실제 gate로 사용해야 한다.

### R06 — 공통 atomic_write의 고정 temp가 다른 파일을 따라 기록

[`atomic_write`](../../crates/core/src/fs.rs:12)는 `<target>.deppytmp`에 `std::fs::write`하고 rename한다. 기존 temp가 링크이면 링크 대상을 truncate하고, 링크 자체를 최종 설정 경로로 옮긴다. 실제 호출자는 앱 config(`config.rs:740`), Claude/Codex hook 설정(`agent_hooks.rs:151/244`), Kimi 설정(`agent_shim.rs:449/467`), shell 통합 생성(`env_reload.rs:389`)이다. `env_reload` 대상은 프로젝트 `.env`가 아니라 Deppy의 zdot shell 초기화 파일이다.

actual helper include probe: 미리 만든 fixture temp link의 다른 파일 내용이 `new config`로 바뀌고, 최종 config도 링크가 됐다. 같은 helper의 4-thread 동시 저장 120회 중 90회 rename 오류도 확인했다. **0 PASS / 2 FAIL**, `atomic-write.log`. 동시 same-path writer의 실제 제품 호출 순서는 입증하지 못했으므로 별도 제품 finding으로 세지 않는다. payload 혼합도 주장하지 않는다. 고유 temp를 `create_new`로 만들고, output 객체가 링크를 따르지 않게 해야 한다.

### R07 — 비동기 종료 쓰기가 재실행된 세션 상태를 되돌림

[`session_exited`](../../crates/runtime/src/persistence.rs:276)는 기존 persistent UUID의 exited 상태를 worker에 enqueue한다. [`session_respawned`](../../crates/runtime/src/persistence.rs:248)는 같은 UUID를 동기 upsert로 running으로 바꾼다. 나중에 batch의 조건 없는 status UPDATE(`storage/write_worker.rs:509`)가 실행되면 running이 exited로 되돌아간다.

actual `PersistPipe`와 isolated SQLite에서 exit enqueue→동일 UUID respawn→worker flush 후 **DB `exited`, 기대 `running`**을 확인했다. **1 FAIL**, `runtime-first.log`. fixture flush interval을 3600s로 늘려 순서를 결정적으로 만들었고 실제 flush API로 배출했다. 기본 interval 자체가 3600s라는 뜻이 아니다. 메모리의 live 상태까지 같은 방식으로 바뀐다고 주장하지 않는다. 동일 UUID의 모든 쓰기를 순서화하거나 execution generation을 fence해야 한다.

### R08 — 읽은 링크 대상과 저장 대상이 다름

load/current revision은 링크를 따라가지만 [`atomic_replace`](../../crates/app/src/document_io.rs:355)는 lexical path의 링크 엔트리를 교체한다. App은 open/save에 lexical path를 유지한다(`app.rs:21963/21732`). 결과는 `Saved`여도 원래 대상에는 수정이 반영되지 않는다.

actual load/save: alias에서 `old source`를 읽고 저장했더니 alias는 regular file의 `new source`, target은 `old source`였다. **exit 101**, `document-symlink.log`. R03의 외부 변경 race나 R06의 fixed temp와 독립적인 원인이다. 열린 대상 identity를 보존하고 저장에도 사용하거나, 링크 편집을 명시적으로 거부해야 한다.

### R09 — interactive shell의 foreground job이 freeze에서 빠짐

[`freeze`](../../crates/pty/src/lib.rs:1924)와 resume은 spawn 때의 shell process group만 signal한다. interactive shell의 job control은 foreground 외부 작업을 다른 PGID로 만든다. runtime은 호출 성공만으로 frozen=true 이벤트를 보낸다(`runtime/in_process.rs:3306`).

actual private zsh PTY에서 별도 foreground job을 만든 뒤 freeze했다. **shell `Ts`, foreground `S+`**, freeze는 `Ok`. 500ms 동안 foreground가 stop 상태에 들어가지 않았고 assertion 실패했다. cleanup은 fixture foreground와 fixture Session에만 적용했다. **0 PASS / 1 FAIL**, `foreground-freeze.log`. 현재 workload의 실제 그룹/identity를 추적하여 정지·재개하고 성공을 검증해야 한다.

### R10 — 종료 후 지연 처리된 터미널 응답이 후속 셸로 들어감

[`agent_wrapper`](../../crates/runtime/src/agent_wrapper.rs:46)는 terminal query가 agent 생존 중 처리되어 응답이 이미 input queue에 있다고 가정한다. time-0 drain은 당시 버퍼만 비운다. 실제 reader→Session pump는 비동기이며 [`Session::pump`](../../crates/session/src/session.rs:360)는 뒤늦게 파싱한 query의 응답을 같은 PTY에 다시 쓴다. 그때는 fallback shell이 읽고 있을 수 있다.

actual production wrapper/Session/PTY probe에서 agent가 CPR query `ESC[6n`를 출력하고 종료한 후 fallback reader 준비까지 pump를 늦췄다. fallback에서 응답 **`1b 5b 31 3b 31 52`**를 받았다. **0 PASS / 1 FAIL**, `late-query-confirmed.log`. 실제 사용자 키를 주입하거나 외부 agent를 실행하지 않았다.

첫 harness는 macOS `od`의 double-space 포맷을 single-space 문자열로 비교해 false PASS였다. 실제 output 진단 후 whitespace token 비교로 교정해 위 RED를 얻었다. `runtime-first.log`와 `late-query-diagnostic.log`도 보존했다. 새 에이전트/셸의 legitimate 응답까지 무조건 막지 않고, 종료 boundary와 query 응답 소유권을 동기화해야 한다.

### R11 — 편집 입력은 load/save와 전체 문서 budget 밖에서 무제한 성장

[`source_editor`](../../crates/app/src/ui/document.rs:276)는 TextEdit 입력 상한이 없다. `app.rs:21639`는 편집 후 dirty/revision만 갱신한다. load의 1MiB editable/8MiB refuse 기준과 24MiB pool admission은 편집 insert에 적용되지 않는다. >1MiB 편집 보존과 경고는 기존 의도이므로 그것만 결함으로 세지 않았다.

actual egui와 byte-identical production editor 함수: 9MiB paste를 전부 보유해 absolute 8MiB save 상한을 넘었다. **exit 101**, `editor-paste.log`. `a\n` 약 470만 줄의 극단 fixture에서 paste/layout은 **5.270s**였다. headless/debug artifact 측정이며 일반 문장이나 release 앱 지연으로 일반화하지 않는다. 삽입 전 byte/aggregate budget과 renderer 비용 한계가 필요하다. 한도를 넘는 붙여넣기는 원래 문서와 clipboard 내용을 손실시키지 않는 UX로 처리해야 한다.

### R12 — import 완료 전달의 backpressure가 소유권과 결과를 소실

[`dispatch`](../../crates/connector-service/src/coordinator.rs:564)는 Complete/FailImportSource 소유권을 `take_matching`으로 먼저 소비한다. enqueue가 Full이면 command를 drop한다(`:701`). App도 완료 dispatch 오류를 warning만 남기고(`app.rs:16006`), host task는 이미 take/join되어 완료 payload를 보존하지 않는다(`:16290`).

기존 actual coordinator fixture의 repository open gate로 dequeue를 잠시 막고 실제 8-slot command queue를 채웠다. completion 결과는 **Backpressure**, pending owner **false**. gate를 열어 drain한 뒤 같은 완료를 다시 전달하면 **StaleHostAction**, parses/imports **0**이었다. **0 PASS / 1 FAIL**, `connector-import-queue.log`. file picker·실제 파일/키/외부 transport는 사용하지 않았다. 소유권은 enqueue 성공 후 소비하고 App은 재전달 가능한 bounded completion을 보존해야 한다. worker 불가·cancel·stale epoch도 같은 설계에서 다뤄야 한다.

### C01 — non-Unix 정상 폴더도 설정 불가: 조건부 정적 결함

최근 [`SetProjectPath`](../../crates/app/src/app.rs:5382) 변경은 정상 nonempty path에도 identity probe 성공을 필수로 요구한다. [`folder_identity`](../../crates/app/src/folder_identity.rs:51)의 non-Unix probe는 항상 None이다. 따라서 해당 cfg의 정상 폴더 설정도 ProjectPath 오류가 된다. 이전 코드는 identity 없는 path 저장을 허용했다.

Windows용 기존 코드가 있지만 현재 native App CI는 macOS만 검증한다. Windows 실행·릴리스 지원 범위는 이번 검토에서 입증하지 못했으며 macOS 결함 12건과 분리했다. cross-platform 계약을 유지하려면 플랫폼별 유효 경로 판정과 identity 지원 여부를 구분해야 한다. non-macOS permanent-delete fail-closed는 기존 QA에 명시한 의도라 새 finding으로 세지 않았다.

### C02 — Relay receive의 idle timeout은 전체 frame deadline이 아님: 현재 활성화 BLOCKED

[`receive`](../../crates/web-remote/src/relay_client/tls.rs:201)는 socket idle read timeout만 설정하고 synchronous tungstenite message read를 호출한다. timeout보다 짧은 간격으로 message bytes가 오면 함수 전체가 deadline을 넘는다. worker의 stop flag는 receive 이후에야 확인된다(`worker.rs:356`). App `relay_disable`은 UI에서 join한다(`app.rs:22820`).

actual TLS source를 byte 그대로 복사하고 private access helper만 추가했다. loopback `MaybeTlsStream::Plain` 경로에서 40ms budget의 receive가 252ms 걸리는 것을 확인했다. actual `worker.rs/lifecycle.rs`와 real session을 사용한 shutdown probe도 **1s receive slice인데 3.108s shutdown**, assertion 실패였다. transport establishment만 Ready fixture였고 public TLS/인증/배포는 검증하지 않았다. `relay-receive.log`, `relay-shutdown.log`.

현재 production endpoint/route/credential은 None/BLOCKED여서 이 Relay 연결을 실제 사용자 앱의 현재 멈춤 원인으로 주장하지 않는다. 전체 deadline 또는 shutdown 시 socket 취소가 필요하며 Relay 활성화 전 수정 항목이다. 초기 connect에도 같은 종류의 total-deadline 후보가 있으나 실제 재현하지 않아 별도 finding으로 세지 않는다.

### P01 — process snapshot adjacency 반복 구축: 낮은 우선순위 최적화

[`descendant_pids`](../../crates/app/src/agent_detect.rs:2328)는 매 호출 전체 rows에서 children map을 만든다. cold/no-agent pass는 agent_kinds, others, find_agent 경로에서 동일 snapshot을 세션당 대략 세 번 사용한다(`:696/697/778`). snapshot당 adjacency를 한 번 생성하면 반복 비용을 줄일 수 있다.

production 함수 본문을 추출한 `rustc -O` synthetic harness의 Root 재실행 평균: 32 sessions/2,048 rows **1.340ms**, 128/8,192 **18.448ms**, 256/16,384 **71.492ms**. 각 조건 10 pass이며 rows는 합성 데이터다. process 실행/분류/transcript I/O와 앱 전체 frame은 측정하지 않았다. 워커 CPU 최적화 후보이지 현재 사용자의 UI 정지를 설명한 결과가 아니다. `process-detect-bench.log`.

## 권장 수정 묶음

1. **PR 종료·정지 보장:** R01과 R09. blocking write 취소, child/process identity·foreground 그룹, bounded join과 성공 표시를 각각 회귀로 검증.
2. **PR 비정규 파일·UI I/O:** R02. document lane과 usage worker의 regular-file/byte/time 기준을 통일.
3. **PR 문서 저장 무결성:** R03/R08. 외부 편집 충돌과 링크 target identity를 별도 시나리오로 검증.
4. **PR 문서 수명·입력 budget:** R04/R11. 세션 독립성 유지, dirty 닫기·복구, 개별/전체 paste budget.
5. **PR 공통 atomic 저장:** R06. 고유 temp·exclusive create 및 실패 cleanup/동시 저장 검증.
6. **PR DB backup·상태 순서:** R05/R07. SQLite snapshot backup과 execution generation/쓰기 직렬화는 각각 독립 회귀로 검증.
7. **PR 후속 셸 응답 소유권:** R10. 지연 pump·종료·새 agent/shell 전환 및 legitimate input 보존.
8. **PR import 완료 보존:** R12. queue full/retry/cancel/stale epoch/worker unavailable/App completion 보존.
9. **지원 범위 확정 후 별도 PR:** C01/C02. non-Unix 폴더와 Relay의 현재 미활성 경계.
10. **후속 성능 PR:** P01. snapshot adjacency reuse 후 실제 탐지 worker workload에서 전후 비용 비교.

위 PR은 권장 범위이며 생성·구현·커밋하지 않았다. 현재 요청은 리뷰다.

## 누락·제한과 제외한 후보

- 실제 OS sleep/reconnect, IME/GUI 모든 입력, 외부 provider 로그인·API, actual mobile/PWA 실기기, Linux/Windows 및 Ghostty 실행은 이번 검증 범위 밖이다. 버그가 없다고 보증하지 않는다.
- 기존 popup R1–R10와 wide-cursor 수정은 현재 baseline에 반영돼 있다. 같은 현상을 근거 없이 재보고하지 않았다. C01만 strict path guard가 non-Unix에 미치는 남은 조건으로 분리했다.
- bounded Git 512 rows/history 256 rows의 단순 필터를 병목으로 세지 않았다. history sparse context 반복 scan/sort는 의미 있는 비용을 입증하지 못해 제외했다.
- 일반 `ps`/storage best-effort 오류, detached child teardown, Relay 초기 connect 시한 등의 추가 가설은 충분한 생산 경로/재현 근거가 없어 확정 finding으로 늘리지 않았다.
- 첫 scratch 복사 cwd 오류, diagnostic script의 rustfmt 줄바꿈 매칭 실패, harness의 type/API 컴파일 오류는 제품 버그로 세지 않았다. 해당 준비 실패 이후 실제 compile/run 결과만 위에 사용했다.

최종 제품 SHA/diff 보존 결과와 문서/evidence 검증은 [handoff](../CODEX_HANDOFF.md) 및 evidence manifest에 기록했다.
