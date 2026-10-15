# 전체 코드 리뷰 재현 근거

대상은 canonical performance working tree의 HEAD d8826f0b + 직전 미커밋 수정, source/test 0.12.1이다. 제품 소스와 기존 테스트는 read-only였다. Root가 Cargo를 단일 슬롯으로 실행했고 서브에이전트는 source/standalone fixture만 사용했다.

`manifest.json`은 원본 private temp 경로·원본 SHA256·저장 SHA256를 기록한다. 로그/소스에서 행 끝 공백과 마지막 빈 줄만 정리했다. 테스트 결과·payload·숫자를 고치지 않았다. 실행 binary는 저장하지 않았다.

## 실제 실행 결과

| 근거 | 실행 결과 | 보호 기대와 실제 결과 |
| --- | --- | --- |
| workspace-tests.log | exit 0, 2238 PASS / 18 ignored | 나머지 24 workspace package 기존 검사 |
| codex-shutdown.log | exit 101, 1 FAIL | 실제 client shutdown이 fixture child 종료 전 완료 못함 |
| document-fifo.log | exit 101 | 문서 lane 정체·정상 admission Full |
| usage-fifo.log | exit 101 | UI usage 함수가 FIFO에서 250ms 이상 대기 |
| document-save-race.log | exit 101 | 외부 수정 후에도 Saved·내용 손실 |
| document-orphan.log | exit 101 | actual aux state/helper에 dirty 접근 경로 없음 |
| document-symlink.log | exit 101 | Saved·alias regular·target old source |
| editor-paste.log | exit 101 | 9MiB 보유·debug paste/layout 5.270s |
| atomic-write.log | exit 101, 2 FAIL | temp link 대상 덮임·동시 rename 90/120 오류 |
| backup-production.log | exit 101 | actual storage-core migration 성공·backup row 누락 |
| runtime-first.log | exit 101, 1 PASS / 1 FAIL | respawn DB exited; late query의 잘못된 문자열 비교 |
| late-query-diagnostic.log | exit 0, 1 PASS | actual od output에 double spaces; harness false PASS |
| late-query-confirmed.log | exit 101, 1 FAIL | token 비교 교정 후 fallback이 CPR 응답 수신 확인 |
| foreground-freeze.log | exit 101, 1 FAIL | 실제 shell Ts·foreground S+ |
| connector-import-queue.log | exit 101, 1 FAIL | Full completion 이후 pending false/retry stale/import 0 |
| relay-receive.log | exit 101 | 실제 socket message read가 전체 budget 초과 |
| relay-shutdown.log | exit 101 | real worker shutdown 3108ms·slice1000ms |
| process-detect-bench.log | exit 0 | optimized synthetic 함수 비용·실제 앱 벤치 아님 |

Root는 모든 주요 binary/test를 직접 실행했다. `relay-receive.log`는 서브에이전트 실행 결과이고 Root는 강화된 실제 worker shutdown을 재실행했다. source-only C01은 Windows 실행 결과가 없다. 서로 다른 probe를 duplicate PASS/FAIL 합계로 묶지 않았다.

## Cargo 재현

`reproductions-tests-only.patch`는 canonical 현재 제품 소스와 scratch의 차이다. 새 회귀는 모두 cfg(test)에 있다. 기존 dirty 제품 patch는 `products-before.patch`에 별도로 보존했다. patch를 canonical에 자동으로 적용하지 않는다.

원래 실제 scratch는 `/private/tmp/deppy-full-review-worktree-20261014`였다. 해당 scratch에서 다음 명령을 **차례로** 실행했다. `CARGO_TARGET_DIR`는 캐시 공유용이고 scratch의 crate entry만 touch해 0 selected/stale artifact를 피했다.

```sh
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target CARGO_BUILD_JOBS=2 cargo test --offline --locked -p runtime review_20261014_ -- --nocapture --test-threads=1
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target CARGO_BUILD_JOBS=2 cargo test --offline --locked -p deppy-sijo --bin deppy-sijo full_review_20261014_codex_stalled_stdin_shutdown_is_bounded -- --nocapture --test-threads=1
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target CARGO_BUILD_JOBS=2 cargo test --offline --locked -p pty full_review_20261014_freeze_stops_foreground_job_not_only_shell -- --nocapture --test-threads=1
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target CARGO_BUILD_JOBS=2 cargo test --offline --locked -p runtime full_review_20261014_late_agent_query_cannot_reach_fallback_shell -- --nocapture --test-threads=1
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target CARGO_BUILD_JOBS=2 cargo test --offline --locked -p connector-service all_review_20261014_import_completion_survives_command_backpressure -- --nocapture --test-threads=1
```

원래 첫 runtime 결과는 late query harness 교정 이전이다. 저장한 최종 patch로 재실행하면 두 runtime 보호 기대가 모두 실패하는 것이 현재 버그의 예상 결과다. fixture cleanup은 자체 생성한 child/PGID/PTY/DB만 사용한다. 실제 앱을 launch하거나 실제 provider를 호출하지 않는다.

## standalone 재현

소스는 원래 canonical 경로를 `#[path]`로 참조하거나 생산 함수/전체 모듈을 그대로 복사했다. production source와 helper seam을 구분한다. `$HOME`/`$CODEX_HOME`을 바꾸지 않는다. 실제 compile에 사용한 dependency artifact는 당시 target cache에 남아 있다. 새 build 환경에서는 해당 cohort를 확인해야 하며 0 selected/컴파일 오류를 행동 결과로 세지 않는다.

실제 document probe compile과 실행:

```sh
rustc --edition=2024 /private/tmp/deppy-ui-doc-review-20261014.rs -o /private/tmp/deppy-ui-doc-review-20261014 -L dependency=target/debug/deps --extern uuid=target/debug/deps/libuuid-39e686ea66bc565f.rlib --extern tracing=target/debug/deps/libtracing-d2a492c6e2ad3d34.rlib --extern libc=target/debug/deps/liblibc-479c45172814a216.rlib
/private/tmp/deppy-ui-doc-review-20261014 fifo
/private/tmp/deppy-ui-doc-review-20261014 symlink
/private/tmp/deppy-ui-doc-review-20261014 save-race
```

실제 추가 standalone 명령:

```sh
rustc --edition=2024 /private/tmp/deppy-ui-session-review-20261014.rs -o /private/tmp/deppy-ui-session-review-20261014
/private/tmp/deppy-ui-session-review-20261014
rustc --edition=2024 /private/tmp/deppy-ui-editor-review-20261014.rs -L dependency=target/debug/deps --extern egui=target/debug/deps/libegui-7162ae282639b4a3.rlib -o /private/tmp/deppy-ui-editor-review-20261014
/private/tmp/deppy-ui-editor-review-20261014
rustc --edition=2024 /private/tmp/deppy-full-review-usage-fifo-20261014.rs -L dependency=target/debug/deps --extern libc=target/debug/deps/liblibc-e8b4ceaeb7c818d7.rlib --extern serde_json=target/debug/deps/libserde_json-470d943c96af7edd.rlib --extern deppy_core=target/debug/deps/libdeppy_core-a764e6eca9426b7c.rlib -o /private/tmp/deppy-full-review-usage-fifo-20261014
/private/tmp/deppy-full-review-usage-fifo-20261014
rustc --edition=2024 --test /private/tmp/deppy-atomic-write-audit-20261014.rs -o /private/tmp/deppy-atomic-write-audit-root-20261014
/private/tmp/deppy-atomic-write-audit-root-20261014 full_review_atomic_write_ --nocapture --test-threads=1
rustc --edition=2024 /private/tmp/deppy-all-review-remote-data-20261014/backup-production.rs -L dependency=target/debug/deps --extern anyhow=target/debug/deps/libanyhow-c83e308520a2d00c.rlib --extern rusqlite=target/debug/deps/librusqlite-b9d6ba29a6f72adc.rlib -o /private/tmp/deppy-all-review-remote-data-20261014/backup-production
/private/tmp/deppy-all-review-remote-data-20261014/backup-production
rustc -O --edition=2024 /private/tmp/deppy-detect-descendants-bench-20261014.rs -o /private/tmp/deppy-detect-descendants-bench-20261014
/private/tmp/deppy-detect-descendants-bench-20261014
/private/tmp/deppy-all-review-remote-data-20261014/relay-shutdown-production
```

Relay source prefix와 canonical tls.rs 전체가 byte 동일함을 Root가 확인했다. suffix는 private access fixture helper다. worker/lifecycle는 `#[path]`로 production을 사용하고 transport establishment만 Ready seam이다. session은 실제 tungstenite/loopback Plain socket이며 public TLS/auth/deployment 성공을 뜻하지 않는다.

`document-orphan-probe.rs`는 actual SessionAuxViews와 생산 helper/caller filter를 사용하지만 UI port type은 stub이다. actual Workspace X/App E2E로 과장하지 않는다. editor/usage/descendant는 함수 본문이 생산 소스와 동일하고 fixture wiring만 별도다. benchmark는 최적화된 합성 함수 비교이고 editor는 headless/debug 극단 줄 수의 측정이다.

## 보존 검증

`source-before.json`의 412개 파일을 최종 SHA256와 비교해 changed/missing 0을 확인했다. canonical `git diff --binary -- crates web xtask Cargo.toml Cargo.lock`도 `products-before.patch`와 byte 동일했다. version bump/배포는 하지 않았다. 실제 릴리스에는 별도 version increase와 bundle/binary 검증이 필요하다.

`final-verification.json`에 최종 source/diff 보존과 evidence/link/whitespace 및 실제 broad 집계 검사 결과를 저장했다.
