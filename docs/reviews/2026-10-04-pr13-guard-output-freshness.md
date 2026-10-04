# PR13 — 입력 승인 직전 PTY 출력 최신화

## 목적과 범위

- 기준: `f64d8d9c1b9fac10a0e02756b97dacbb3abcb8ac`.
- 런타임이 PTY 출력을 pump하기 전에 명령을 처리해, 이미 대기 중인 선택 대화상자나 DEC2004 해제 출력이 승인 검사에서 빠지는 문제를 수정한다.
- 소유 파일: runtime 입력 승인/출력 pump, 필요한 Session/PTY 출력 수신 helper, 이 검토 기록. App/UI, 버전, lockfile, 루트 handoff는 변경하지 않는다.

## 재현 — 실제 실행

모든 Cargo 실행은 `/private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py`의 공유 잠금과 source-owner 정리를 사용한다.

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -p runtime pr13_guard_refreshes_queued -- --nocapture
```

- RED: 테스트 소유의 실제 `/bin/sh` → `/bin/cat` 세션에서 출력 wake로 큐 적재를 확인했다. 기존 viewport/mode는 그대로인 상태에서 입력 승인 경계를 호출했다.
- 두 재현 모두 실제 `Ok(())`, 기대 `Err(AdmissionDenied)`로 실패했다: DEC2004 해제, 빈 입력줄 위의 선택 안내.
- 첫 실행의 DEC fixture는 Rust 문자열의 octal 표기 때문에 NUL을 만들어 spawn에서 실패했다. shell script를 raw 문자열로 바로잡은 뒤 위 명령을 다시 실행하여 두 테스트의 의도한 RED를 확인했다.
- 초기 수정 후 같은 명령: 2 passed, 0 failed. 이후 대기 중 출력/홍수/키보드와 수신 예산 회귀를 추가했다. 최종 검증은 아래에 기록한다.
- 추가 5개 runtime 회귀 첫 실행: 4 passed, 1 failed. 선택 화면 테스트의 raw byte 수를 원문 23 byte로 가정했지만 PTY의 ONLCR 변환으로 실제 25 byte였다. 소비된 실제 byte 수를 기록한 뒤 다음 normal pump에서 동일한지 확인하도록 fixture 단정만 수정했다.

## 구현 결정

- 자동/AI guard 또는 bracketed paste 요구가 있는 승인만, authorizer가 허용한 callback **안에서** 원래 세션의 출력을 한 번 최신화한다. authorizer 대기 이전에 읽은 상태를 재사용하지 않는다.
- guard pass는 엄격한 256 KiB 예산이다. 남은 예산보다 큰 앞 청크는 큐에 유지한다. backlog가 남으면 body와 CR 모두 미전송으로 거절하고 자동 재시도하지 않는다.
- 기존 parser, OSC mark, detector 평가를 하나의 공통 경로로 유지한다. guard에는 열린 로그가 있고 실제 출력이 있을 때만 단일 raw Vec를 만들며, 용량을 정확히 256 KiB로 예약한다. chunk 수와 관계없이 raw body charge는 256 KiB 이하이고, 출력이 없으면 body allocation도 없다. redaction과 log batch 디스크 쓰기는 승인 잠금이 풀린 뒤 수행한다. normal pump의 기존 즉시 batch는 유지한다.
- Root 자체 검토가 초기 구현의 log append/compaction이 승인 잠금 안에 있음을 지적했다. 디스크 지연이 취소를 막을 수 있어 위의 bounded deferred log로 바로잡았다. 이벤트/영속 상태/종료 cache·archive·pane 정리도 승인 잠금이 풀린 후 수행한다.
- guard와 출력 처리가 걸린 시간을 반영해 실제 입력 큐 직전에 deadline을 다시 확인한다. 검사 중 새 출력이 적재돼도 재시도 loop 없이 미전송 거절한다.
- guard 없는 일반 키보드 입력은 기존 경로를 사용한다. 새로운 전역 polling이나 앱 실행은 없다.
- 실제 watched pane의 빠른 종료에서 display cadence가 미뤄지면 final viewport 없이 pane이 닫히는 별도 원인을 확인했다. `pump_sessions(false)`와 실제 `SessionExited` 경계를 사용하는 재현이 RED였다. 종료 시 watched/remote-viewed dirty 화면만 effects에 기록해, 승인 잠금 밖의 finish에서 마지막 snapshot을 만들고 pane 정리 전에 발행한다. hidden snapshot이나 추가 drain은 만들지 않는다.

## 최종 검증

- 최신 focused gate: runtime PR13 7 passed / 0 failed (paced fast-exit 포함), PTY strict receiver 1 passed / 0 failed.
- 새 delayed log sink는 실제 입력 승인 helper를 사용하고, 디스크 단계의 bounded channel handshake 중 별도 control thread의 permit revoke가 완료되는지 확인한다. collector의 로그 offset 불변, 실제 deferred raw 용량 256 KiB, 출력 없는 pass의 allocation 없음도 확인했다.
- 첫 전체 기본 병렬 실행: PTY 45 passed / 1 ignored; runtime 331 passed / 2 failed. `resource_monitor::tests::repeated_capture_cycles_leave_no_reader_growth`는 process-wide reader count 2를 0으로 기대했고, `spawn_agent_secret_env_주입`은 이벤트 대기 timeout이었다. reader-count 단정은 이 fixture의 lock 밖에서 실행되는 다른 runtime capture도 포함한다. 이어진 serial 실행에서 reader-count는 통과했고 runtime 333 passed / 1 failed (동일 viewport timeout), PTY 45 passed / 1 ignored였다. 두 gate 모두 실패에서 중단돼 session/Clippy/fmt check를 실행하지 않았다. 해당 resource-monitor 코드나 기존 secret fixture는 변경하지 않았다.
- 같은 secret fixture의 exact serial 단독 실행: 수정 중 소스에서 1 passed / 0 failed. baseline `f64d8d9c`를 별도 detached test worktree에 두고 동일 gate/명령으로 실행한 결과 0 passed / 1 failed, 동일 Probe timeout이었다. 따라서 PR13에서 새로 만든 secret fixture 실패로 단정하지 않았다.
- fast-exit RED 명령: `cargo_gate.py test --offline --locked -p runtime in_process::tests::pr13_final_watched_output_survives_paced_fast_exit -- --exact --nocapture --test-threads=1`. 실제 1개가 실행되어 final viewport 누락 단정으로 실패했다. 앞선 short-name + `--exact` 실행은 0개였고 RED로 계산하지 않는다. fix 후 `cargo_gate.py --batch '[["fmt","--all"],["test","--offline","--locked","-p","runtime","pr13_","--","--nocapture","--test-threads=1]]'`: 7 passed / 0 failed. 앞선 수동 JSON batch 오타는 Cargo 실행 이전에 JSON parser 오류였고 검증 결과로 계산하지 않는다.
- finite tail 경계의 실제 구현도 확인했다: byte budget 소진은 PTY를 남기고 다음 tick에서 계속 drain하며, EOF + child exit 또는 40 grace tick에서 종료한다. Unix exit 관찰은 reader의 drain-before-slave-close를 깨운다. 확인된 finite tail 유실이 없어 추가 추정 수정은 하지 않았다.
- 최종 관련 전체 serial gate exit 0: runtime **335 passed / 0 failed** (기존 secret fixture 포함), session **74 passed / 0 failed**, PTY **45 passed / 0 failed / 1 existing ignored**. 세 패키지 doc-test는 각각 0개였다. 같은 gate의 세 패키지 all-target strict Clippy `-D warnings`와 `fmt --check`도 exit 0이었다. `git diff --check` exit 0.

최종 gate의 정확한 명령:

```sh
cd /private/tmp/deppy-audit-pr13-20261004
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-p","runtime","-p","session","-p","pty","--","--test-threads=1"],["clippy","--offline","--locked","-p","runtime","-p","session","-p","pty","--all-targets","--","-D","warnings"],["fmt","--all","--","--check"]]'
git diff --check
```

Baseline 단독 재현의 정확한 명령:

```sh
cd /private/tmp/deppy-audit-pr13-base-probe-20261004
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -p runtime in_process::tests::spawn_agent_secret_env_주입 -- --exact --nocapture --test-threads=1
```

첫 PR13 단계의 출력은 실행 도구의 stdout으로 받았으며 별도 `/tmp` raw log 파일로 redirect하지 않았다. 해당 단계의 보존된 결과 기록은 이 파일이다. 아래 teardown 후속 수정에서는 raw log를 별도로 보존한다. source-owner가 바뀔 때 gate가 workspace artifact를 정리한 baseline 비교였으며, 기존 fixture나 resource-monitor 단정은 수정하지 않았다.

## 후속 검토 — 종료 PTY destructor의 승인 잠금

- 독립 CLI 결과 `/tmp/deppy-corrected-core-cli-result-20261004.txt`의 medium finding을 실제 `Session::pump_inner` → `self.pty = None` → `PortablePtySession::drop` 경로에서 확인했다. 살아 있는 descendant가 HUP/TERM을 무시하면 200 ms escalation과 worker join이 기존 guard callback의 permit/credential 잠금 안에서 실행될 수 있었다.
- 실제 RED: 테스트 소유 `/bin/sh` parent가 최종 출력 뒤 종료하고, descendant가 HUP을 무시하며 TERM trap에서 teardown-start marker를 쓴다. 별도 control thread는 같은 captured permit을 revoke하고, descendant가 그 revoke 완료 marker를 확인한 뒤 ack를 쓰고 종료한다. 기존 경로는 teardown-start 시 authorizer가 `Some(true)`였고 기대 `Some(false)`로 실제 1개 테스트가 실패했다. `/tmp/deppy-pr13-teardown-red-20261004.log`에 raw output을 보존했다.
- 수정: strict guard pump는 종료 PTY를 `Session.pty.take()`로 즉시 분리하고 ownership을 PumpResult와 함께 effects로 옮긴다. 따라서 lifecycle은 Exited이고 일반/배치 입력, foreground 조회와 다음 pump에는 PTY가 없다. destructor/escalation/join은 `InputAdmission::admit`가 반환한 뒤 finish 첫 단계에서 실행한다. normal pump는 기존 위치에서 즉시 drop한다. 승인 거절/오류도 refresh effects를 finish하는 기존 단일 반환 경로를 유지한다.
- focused GREEN: runtime PR13 **8 passed / 0 failed**, raw log `/tmp/deppy-pr13-teardown-focused-green-20261004.log`. 실제 teardown 시작 시 authorizer가 false이며, permit revoke 완료 후 descendant ack가 destructor 완료 전에 남았음을 확인했다. 실제 Session이 deferred PTY ownership을 보유하는 동안 DEC2004가 on이어도 일반/배치 입력이 None이고 foreground 조회가 None인 추가 경계 테스트를 넣었다.
- 최종 관련 serial gate `/tmp/deppy-pr13-teardown-final-gate-20261004.log`: 실제 Session closed-writer focused **1 passed / 0 failed**, PTY 전체 **45 passed / 0 failed / 1 existing ignored**, runtime 전체 **335 passed / 1 failed**였다. 유일한 실패는 위에서 baseline exact 실패도 확인한 기존 `spawn_agent_secret_env_주입`의 동일 Probe timeout이다. 새 runtime PR13 8개는 이 전체 실행에서도 모두 통과했다. 해당 gate는 runtime 실패에서 멈췄으므로 Session 전체/Clippy/fmt check는 실행하지 않았다. 실패를 전체 통과로 계산하지 않는다.
- 후속 gate `/tmp/deppy-pr13-teardown-final-continuation-20261004.log` exit 0: Session 전체 **75 passed / 0 failed**, 세 관련 패키지 all-target strict Clippy `-D warnings`와 `fmt --check` exit 0, 기존 secret fixture exact **1 passed / 0 failed**였다. 최종 `git diff --check` exit 0. 기존 fixture, mock store, resource-monitor 코드/단정은 변경하지 않았으며 실패 원인을 추정하여 제품 수정을 더하지 않았다. 이 단독 통과를 앞선 runtime 전체 실패 대신 계산하지 않는다.
- Root 요청에 따라 기존 synthetic secret fixture에만 실패 진단을 추가했다. 성공 조건/timeout은 그대로이며 실패 시 seen event의 종류, own first row와 다른 row의 기대 문자열 존재 여부, SpawnFailed의 정적 코드, 동일 테스트 credential의 resolve 일치 여부만 출력하고 원래 panic을 다시 전달한다. 정상 경로에는 진단 조회/출력이 없다. generic Probe, 실제 keyring error 문자열, 사용자 데이터는 출력하지 않는다.
- 이 진단 포함 최종 runtime 전체 serial `/tmp/deppy-pr13-secret-fixture-diagnostic-20261004.log`: **336 passed / 0 failed**, doc-test 0개, gate exit 0. 실패가 재현되지 않아 진단은 출력되지 않았고 기존 timeout의 원인이 밝혀졌다고 주장하지 않는다. 직전 실패 로그와 baseline 비교는 그대로 남긴다. 이 exact 최종 소스의 strict 세 패키지 Clippy/fmt gate `/tmp/deppy-pr13-teardown-diagnostic-strict-20261004.log`도 exit 0이다. 새 product/teardown 테스트 소스는 Session 75/PTY 45 검증 이후 바뀌지 않았다.

후속 RED의 정확한 명령:

```sh
cd /private/tmp/deppy-audit-pr13-20261004
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test --offline --locked -p runtime in_process::tests::pr13_exit_teardown_releases_authorization_before_revocation_handshake -- --exact --nocapture --test-threads=1 > /tmp/deppy-pr13-teardown-red-20261004.log 2>&1
```

후속 최종 gate의 정확한 명령:

```sh
cd /private/tmp/deppy-audit-pr13-20261004
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["fmt","--all"],["test","--offline","--locked","-p","session","pr13_","--","--nocapture","--test-threads=1"],["test","--offline","--locked","-p","runtime","-p","session","-p","pty","--","--test-threads=1"],["clippy","--offline","--locked","-p","runtime","-p","session","-p","pty","--all-targets","--","-D","warnings"],["fmt","--all","--","--check"]]' > /tmp/deppy-pr13-teardown-final-gate-20261004.log 2>&1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["test","--offline","--locked","-p","session","--","--test-threads=1"],["clippy","--offline","--locked","-p","runtime","-p","session","-p","pty","--all-targets","--","-D","warnings"],["fmt","--all","--","--check"],["test","--offline","--locked","-p","runtime","in_process::tests::spawn_agent_secret_env_주입","--","--exact","--nocapture","--test-threads=1"]]' > /tmp/deppy-pr13-teardown-final-continuation-20261004.log 2>&1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["fmt","--all"],["test","--offline","--locked","-p","runtime","--","--test-threads=1"]]' > /tmp/deppy-pr13-secret-fixture-diagnostic-20261004.log 2>&1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py --batch '[["clippy","--offline","--locked","-p","runtime","-p","session","-p","pty","--all-targets","--","-D","warnings"],["fmt","--all","--","--check"]]' > /tmp/deppy-pr13-teardown-diagnostic-strict-20261004.log 2>&1
```

## 경계와 남은 작업

- 다른 프로세스가 마지막 backlog 검사 이후 출력하는 것과 입력 enqueue를 완전히 원자적으로 묶을 수는 없다. 이미 큐에 들어온 출력은 관찰하며, 검사 도중 관찰된 새 backlog는 미전송 거절한다.
- 예산은 byte 상한이다. parser, 로그 디스크 I/O, process exit 조회의 벽시계 시간을 보장하지 않는다.
- 자체 source review: guard는 원래 target 한 세션만 pump하고 pending output이 남으면 body/CR 모두 미전송 거절한다. PTY destructor/worker join, 로그 redaction/쓰기, 상태 영속화, 이벤트 wake, cache·archive·pane 종료 정리는 authorizer callback이 반환한 뒤에만 수행한다. final viewport도 같은 finish 단계에서 생성되며 watched/render-active 또는 유효 remote-viewer에 한정한다. 최종 fixture는 실제 subscriber slot Arc를 유지하고 실제 SessionExited 뒤 남아 있는 마지막 슬롯을 확인한다.
- 다음 단계: 범위 한정 커밋, frozen SHA를 root에 전달. release/app 실행은 이 단계에 포함되지 않는다.
