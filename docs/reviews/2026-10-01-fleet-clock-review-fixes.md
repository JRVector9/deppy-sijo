# Fleet 대기 시간 리뷰 3건 수정 결과 — 2026-10-01

## 완료 상태

앞선 [코드 리뷰](2026-10-01-fleet-followup-code-review.md)의 3건을 모두 재현·수정했다. 추가 코드 리뷰에서 확인한 연결부·이벤트 역순 문제도 수정했다. 마지막 소스 리뷰에서 확인된 미해결 문제는 없다. **0.5.0 → 0.5.1** 로컬 릴리스 빌드와 별도 서명 앱·ZIP 검증까지 완료했다. 재실행·앱 종료·커밋·푸시는 수행하지 않았다.

| 원래 문제 | 수정 | 검증 |
| --- | --- | --- |
| 미래 idle 시각 하나가 전체 상태 조회를 실패시킴 | 자료형·키·상태·행·바이트 제한을 유지하고, 같은 snapshot 시각보다 미래인 idle 시각만 표시 대상에서 제외 | 20초 시계 역행을 DB에서 재현; 다른 세션 질문·완료 상태 조회 유지 |
| 훅 없는 새 작업에서 이전 대기 시각 재사용 | 실제 PTY 입력 수락 후 제출 시각만 전달; 세션별 입력 경계 이전 완료 시각을 무효화 | 실제 PTY 수락/거절, 빠른 완료 뒤 늦은 입력 이벤트, Fleet 최초 열기, 숨겨진 workspace·종료 정리 |
| 늦은 질문 해제를 quiet 관측 시각으로 계산 | 완료·실제 질문 해제 시각을 별도로 저장; 둘 중 늦은 시각을 대기 시작으로 사용 | 질문100 → 완료120 → quiet150 → 해제130의 결과130; 여러 질문의 역순 해제 결과140 |

## 실제 코드 변경

- `crates/storage/src/db.rs`: 미래 시각은 시간 불확실성으로 처리하고 정상 상태 projection은 유지한다. nullable `idle_generation` forward migration을 추가했다. 대기 경계 세대와 완료 알림 CAS 세대를 분리하고, legacy idle 시각을 마이크로초 경계로 정규화했다. 기존 notification CAS 의미는 유지한다.
- `crates/storage/src/agent_attention.rs`: 완료·질문 해제 경계를 `IdleObserved` watermark와 분리한다. 결과가 질문보다 먼저 저장되면 기존 제한된 요청 이력에서 일치하는 질문/결과 증거를 복원한다. 이름이 있는 요청과 익명 요청 모두 처리하며 무관한 도구 결과는 대기 경계를 바꾸지 않는다. 늦게 도착한 부모 Working/TurnStart보다 새로운 자식 응답 시각을 보존하고, 실제로 더 늦은 새 턴에서는 초기화한다.
- `crates/session/src/status.rs`, `crates/runtime/src/{event,in_process,remote,protocol}.rs`: 기존 bracketed-paste 파서를 사용해 실제 Enter/Ctrl+C 제출 여부를 반환한다. 선택 화면의 기존 대기 상태를 보존하면서 제출 자체는 보고한다. 입력 수락 후 `SessionInputSubmitted`를 enum 끝에 추가하여 전달한다. 이 이벤트에는 세션과 시각만 있고 입력 본문은 없다. paste 내부 개행·입력 거절은 제출로 처리하지 않는다.
- `crates/app/src/ui/{workspace,fleet}.rs`, `crates/app/src/app.rs`: 기존 SessionView의 scalar로 입력 경계를 활성·warm workspace에서 유지하고 세션 정리 때 제거한다. Fleet에 처음 들어오기 전에도 이전 완료 시각을 걸러낸다. 시각 없는 늦은 Running 이벤트가 더 새로운 확정 완료를 지우지 않게 기존 방어를 유지한다.
- 원격 런타임 wire version은 **20 → 21**이다. 새 variant를 모르는 구버전을 hello 단계에서 거부한다. 양쪽 런타임을 같은 버전으로 맞춰야 하며 MCP 공개 도구·프로토콜 버전의 변경은 아니다.
- 화면 프레임의 파일/DB I/O, 별도 폴링, 입력 본문 기록, 새로운 전역 무제한 map을 추가하지 않았다. 기존 팝업 변경 및 원래 worktree는 보존했다.

## 재현 및 추가 리뷰

원래 3건을 제품 테스트에서 실제 RED로 확인한 뒤 GREEN으로 바꿨다. 추가 독립 Codex 소스 리뷰에서 발견한 사항도 테스트로 재현했다.

1. wire variant 추가와 protocol20 유지, 선택 화면 제출 미보고, 알림 완료 세대120과 실제 idle 경계140의 혼용을 수정했다.
2. 자식 해제140이 부모 완료120보다 먼저 저장되는 순서에서 시작 시각120을 저장하던 문제를 수정했다.
3. 결과140이 질문100보다 먼저 저장되거나 부모 Working110이 늦게 저장되는 경우의 시각120 오류를 각각 RED로 확인하고 수정했다. 완료 뒤 늦은 질문, 늦은 TurnStart, 실제 새 턴 초기화도 검증한다.
4. 마지막 테스트는 이름/익명 요청 각각 24개, 합계 **48개 도착 순서**에서 질문100·Working110·완료120·해제140을 조합한다. 추가 무관한 결과150에도 idle 시각140 및 경계140000001을 유지한다. 48개 순서는 하나의 테스트 안에서 실행하며 아래 전체 테스트 수에 48개를 별도로 더하지 않았다.

최종 CLI 리뷰는 최신 reducer와 DB projection의 실제 소스를 읽고 **확인된 남은 결함 없음**으로 종료했다. 이 CLI 단계는 테스트 실행을 하지 않았으며, 아래 테스트는 별도로 직접 실행했다.

## 실제 실행한 최종 검증

작업 디렉터리: `/Users/jr/Desktop/projects/deppy-sijo-performance`.

| 검증 | 실제 결과 | 로그 |
| --- | --- | --- |
| Storage 전체 | 397 passed / 0 failed, 19.28s | `/tmp/deppy-fleet-three-fixes-storage-final-20261001.log` |
| App bin 전체 | 2512 passed / 0 failed / 28 ignored, 6.10s | `/tmp/deppy-fleet-three-fixes-app-final-20261001.log` |
| Runtime 전체 | 318 passed / 0 failed, 23.32s | `/tmp/deppy-fleet-three-fixes-runtime-final-20261001.log` |
| Session 전체 | 72 passed / 0 failed, 0.66s | 위 Runtime/Session 공용 로그 |
| 마지막 clock 회귀 묶음 | 8 passed / 0 failed, 1.24s | `/tmp/deppy-fleet-three-fixes-final-order-green-20261001.log` |
| App/Storage/Session/Runtime Clippy all-targets | exit0, 19.22s | `/tmp/deppy-fleet-three-fixes-clippy-20261001.log` |
| fmt / diff / UI leaf boundary | 모두 exit0 | `/tmp/deppy-fleet-three-fixes-boundary-20261001.log` |
| 최종 reducer 소스 리뷰 | exit0, 확인된 미해결 결함 없음 | `/tmp/deppy-fleet-three-fixes-reducer-final-review-20261001.log` |
| 0.5.1 App/proxy release build | exit0, 25.48s | `/tmp/deppy-fleet-three-fixes-release-20261001.log` |

전체 core 실행 테스트 합계는 **3,299 passed**다. Runtime/Session은 마지막 storage reducer 추가 수정 전에 통과했으며 이후 두 crate의 소스는 바뀌지 않았다. 해당 storage 수정 뒤 Storage/App/정적 검사를 다시 실행했다. 테스트 이후 제품 변경은 Cargo 버전 및 inherited lock 갱신뿐이다. 실제 실행 앱 E2E·프로세스 메모리 실측을 수행했다는 주장은 하지 않는다.

```sh
ulimit -n 4096
cargo test --offline --locked -q -p storage
cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo
cargo test --offline --locked -q -p session -p runtime -F secret/test-keyring-core -- --test-threads=4
cargo clippy --offline --locked -p deppy-sijo -p storage -p session -p runtime --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo run --offline --locked -q -p xtask -- check-boundary
```

## 실패했던 중간 시도

- 최초 Runtime standalone 전체 테스트에서 test-only keyring feature를 빠뜨려 native macOS Keychain 접근 대기와 2개 timeout이 발생했다. 실제 stack sample에 `SecItemCopyMatching`이 있었다. `secret/test-keyring-core` 및 테스트 동시성4를 사용한 재실행은 모두 통과했다. 이 feature는 릴리스 빌드에 넣지 않았다. 종료하려던 테스트는 이미 종료된 상태였고 실행 앱 프로세스는 건드리지 않았다.
- 기본 동시성의 Runtime fixture에서 OS file descriptor 제한에 도달했다. 테스트 프로세스의 `ulimit -n 4096`만 조정했다.
- 두 DB migration 중 마지막 하나만 실행하던 회귀 fixture, 기존 wire20 기대값, 테스트 코드의 moved workspace borrow 오류를 바로잡았다. 불필요한 상수 assertion은 Clippy 지적에 따라 제거하고 실제 hello 거부/codec 회귀를 유지했다.
- `cargo metadata --no-deps`는 lock 버전을 갱신하지 않아 첫 `--locked` 릴리스 명령이 빌드 전에 거부되었다. full offline metadata로 lock을 갱신한 후 27개 workspace/lock 버전 검증 및 실제 릴리스 빌드가 통과했다.

## 릴리스와 소스 식별

- **0.5.0 → 0.5.1** patch 증가. 존재하는 git worktree의 로컬 artifact를 조사한 이전 최대 버전은0.5.0이다. 27개 inherited workspace package와 Cargo.lock 버전을 모두0.5.1로 검증했다.
- 빌드된 실행 파일과 패키지 실행 파일의 `Deppy0.5.1` compiled reported-version marker, `CFBundleShortVersionString`, `CFBundleVersion`을 검증했다. 앱을 실행하지 않고 확인했다.
- 별도 산출물: `target/bundle-0.5.1/Deppy Sijo.app`, `target/bundle-0.5.1/Deppy Sijo.zip`. App/proxy는 새 릴리스 바이너리이며 cloudflared/helper notice·리소스는 이전 검증 번들에서 재사용했다. Developer ID로 helper → main → bundle 순서 서명하고 추출 ZIP까지 signature/architecture/파일 일치 검증을 통과했다. 로컬 개발 검증이며 Apple 공증 완료를 주장하지 않는다.

```sh
cargo metadata --offline --format-version 1
cargo build --offline --locked --release -p deppy-sijo -p mcp-proxy
DEPPY_REQUIRE_TRUSTED_SIGNING=0 DEPPY_ALLOW_UNTRUSTED_SIGNING=1 sh scripts/verify-macos-package.sh 'target/bundle-0.5.1/Deppy Sijo.app' 'target/bundle-0.5.1/Deppy Sijo.zip'
```

- Base commit: `166f8daeb1054cf09a07194fa83bf0a4a19d93ce`, branch `fix/cloud-agent-ended-sessions`. 이번 결과는 미커밋 소스를 포함한다.
- Product diff SHA256: **`ec1a51ae41a7146a9ac51e8cea2a260bd0177d78c30e4967068fc93ee155124c`**. `git diff --binary HEAD`의 Cargo.toml/lock, app/storage/i18n/connector-ui/runtime/session 범위와 정렬된 미추적 제품 파일의 path + NUL + bytes를 결합했다. 기존 팝업 변경도 포함한다.
- 소스/버전/아티팩트 hash 기록: `/tmp/deppy-fleet-fixes-release-source-20261001.json`. 빌드 뒤 제품 소스 hash 불변을 재확인했다.
- 기존 실행 앱은 **0.4.5 / PID45213**으로 유지했다. 승인된 수정·테스트·리뷰·재빌드 작업은 완료했다.
