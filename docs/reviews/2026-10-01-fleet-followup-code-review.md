# Fleet 대기 시간·후속 작업 코드 리뷰 — 2026-10-01

> 2026-10-01 업데이트: 아래 3건 모두 수정·재현 테스트·0.5.1 재빌드를 완료했다. [수정 결과와 최종 검증](2026-10-01-fleet-clock-review-fixes.md)을 참고한다. 아래 본문은0.5.0 리뷰 당시의 증거 기록이다.

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| medium | crates/storage/src/db.rs:1955 | 현재보다 미래인 정상 완료 시각을 전체 조회 오류로 처리 | 시계가 뒤로 바뀌면 다른 세션 상태와 후속 작업 갱신도 지연 | 시간 신뢰도와 행 구조 검증을 분리하고 해당 시각만 관측값으로 전환 |
| medium | crates/app/src/ui/fleet.rs:507 | 훅이 누락된 새 턴에도 이전 완료 시각 재사용 | 실제 작업 시간까지 지시 대기에 포함 | 실제 입력·새 턴 경계와 완료 세대를 연결해 이전 세대를 무효화 |
| medium | crates/storage/src/agent_attention.rs:660 | 조용한 화면 관측 시각을 질문 해제 시각으로 사용 | 늦게 저장된 해제 이벤트에서 대기 시간 과소 표시 | 완료·질문 해제 경계 시각을 관측 watermark와 별도로 관리 |

## 범위와 검토 대상

- 직전 구현의 Fleet 대기 시작 시각, attention 저장·조회·적용, 런타임 이벤트 처리, 다음 작업 표시와 기존 예약 발사 경로를 검토했다. 기존 팝업 전체 변경은 이번 리뷰 범위에 포함하지 않았다.
- 작업 디렉터리: `/Users/jr/Desktop/projects/deppy-sijo-performance`, 브랜치 `fix/cloud-agent-ended-sessions`, HEAD `166f8daeb1054cf09a07194fa83bf0a4a19d93ce`.
- 제품 버전 `0.5.0`, 기존 미커밋 제품 소스 SHA256 `59ba2623add7fbf42ecc4e4c33910553965e790b54ee2d3a2220c0c9969d13a1`을 리뷰 종료 시 재확인했다. 제품 소스는 변경하지 않았다.
- `workstep`의 Codex CLI 리뷰 단계만 사용했다. 자동 수정·커밋·앱 실행은 수행하지 않았다. CLI는 두 시간 정확도 문제를 지적했고, 직접 작성한 별도 테스트로 둘 다 재현했다. 시간 역행 문제는 직접 조사와 테스트로 추가 확인했다.

## Details

### 1. 시스템 시계 역행이 다른 세션 상태 조회까지 막는다

- `IDLE_SESSIONS_PREFLIGHT`는 `idle_since > snapshot_epoch`인 행 하나만 있어도 `bounded read row invalid` 오류로 전체 attention projection을 중단한다. 해당 행이 현재 Fleet에서 살아 있는 세션인지 확인하는 앱 필터보다 앞선 단계다.
- 완료 시각은 `crates/mcp-proxy/src/main.rs:295`의 `SystemTime`에서 생성되고, 저장 함수는 양수 시각을 수용한다. 정상 완료 후 OS 시계가 뒤로 조정되면 이미 저장한 시각이 일시적으로 현재보다 미래가 된다. 이 경우는 구조적으로 손상된 행과 구별할 필요가 있다.
- 재현: 인메모리 DB의 세션 7에 현재보다 20초 이후의 완료 이벤트를 저장해 시계 역행 후의 DB 상태를 구성하고, 세션 8에 정상 질문 대기를 저장했다. projection이 실패했다. 동일 DB에서 세션 7의 `idle_since`만 NULL로 바꾸면 세션 8의 질문 대기가 정상 조회되는 대조 검사도 실행했다.
- 앱은 성공한 Attention 적용 단계에서 `flush_queued_followups()`를 실행한다(`app.rs:16514`). 이 조회 실패는 잘못된 시간 하나의 표시뿐 아니라 다른 세션의 상태 적용과 예약 발사를 지연시킨다. 20초 사례는 시계가 저장 시각을 따라잡으면 해소되지만 큰 역행에서는 더 오래 지속된다.
- 권장: 행 수·문자열 크기·자료형·상태 일관성 제한을 유지하면서 미래 시각의 시간 신뢰도만 낮추고, 다른 정상 상태 행은 계속 투영한다. 단조 시간과 벽시계의 역할도 구분한다.

### 2. 훅 누락 후 새 작업이 이전 완료 시각을 이어 쓴다

- `note_observed_turn_start()`는 `confirmed` 시각을 지우지 않는다(`ui/fleet.rs:422`). `update_idle_clocks()`는 Active 상태에서도 기존 저장 시각이 있으면 이를 먼저 채택한다(`:507`).
- 재현: 완료 시각 100초·세대 100000000을 적용한 뒤, 새 턴의 훅이 없는 상태에서 런타임 Running을 200초에 관측하고 Active → Idle을 230초에 적용했다. 실제 결과는 `(Some(100), true)`였다. 새 조용한 관측 기준은 `(Some(230), false)`이어야 한다.
- 결과적으로 두 번째 작업에서 일한 시간과 그 앞의 대기 시간이 새 턴의 확정된 대기 시간처럼 표시된다. 완료 알림을 확인해도 저장 시각은 보존하므로 알림 확인으로 해결되지 않는다.
- 권장: PTY의 실제 사용자 입력·새 턴을 식별할 수 있는 경계와 완료 세대를 연결한다. 시각·세대가 없는 모든 Running 이벤트를 단순히 리셋하는 수정은 이전에 방어한 늦은 Running 이벤트 문제를 다시 만들 수 있다.

### 3. 늦은 질문 해제가 대기 시작을 조용한 관측 시각으로 밀어낸다

- `state.last_activity`는 `IdleObserved`도 포함한다(`agent_attention.rs:642`). 질문 대기 해제 후 시작 시각을 `last_activity.max(event.at_micros)`로 계산한다(`:662`).
- 재현 순서: `ResponseRequired(q, 100s)` → `Completed(120s)` → `IdleObserved(150s)` → 늦게 저장된 `Resolved(q, 130s)`.
- 질문은 정상적으로 사라졌지만 저장한 `idle_since`는 기대한 130초가 아닌 150초였다. 이후 관측에서는 이 잘못된 시각이 그대로 유지된다.
- 훅 프로세스는 입력 파싱·DB 쓰기 전에 시각을 생성하므로 이벤트 시각 순서와 DB 도착 순서가 달라질 수 있다. 요청 해제 자체도 기존 reducer가 이 순서를 지원한다.
- 권장: 마지막 완료와 마지막 질문 해제의 실제 경계로 시작 시각을 계산하고, 단순 화면 관측 watermark를 정확한 시작 시각 근거로 사용하지 않는다.

## 실제 실행한 검증

제품 소스를 그대로 복제한 detached 리뷰 checkout `/private/tmp/deppy-fleet-user-review-20261001`에만 재현 테스트를 추가했다. 사용자 DB나 OS 시계를 변경하지 않았다. 같은 체크아웃은 재현 증거로 보존했다.

```sh
cd /private/tmp/deppy-fleet-user-review-20261001
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo user_review_probe -- --test-threads=1
CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target cargo test --offline --locked -q -p storage user_review_probe -- --test-threads=1
```

| Test | Result | Evidence |
| --- | --- | --- |
| 훅 없는 새 턴의 이전 시각 재사용 | 실패·문제 재현 | 실제 `(Some(100), true)`, 기대 `(Some(230), false)` |
| 질문 해제보다 늦은 조용한 관측 | 실패·문제 재현 | 실제 `Some(150)`, 기대 `Some(130)` |
| 20초 시계 역행 후 다른 세션 projection | 실패·문제 재현 | `bounded read row invalid`; idle 시각만 제거한 대조 projection은 정상 |
| 긴 예약 프롬프트 원문 툴팁 | 통과 | 실제 클릭 가능한 카드에 hover 후 512자 이후 원문 끝 marker 확인 |
| 긴 관측 대기 표기·모델·목록 경계 | 통과 | 실제 CJK 폰트 설치, 5개 언어, 8시간 44분, warm 카드의 4개 텍스트가 clip rect 안에 존재 |

- 최종 App probe: **2 passed / 1 failed** (0.27s); Storage probe: **0 passed / 2 failed** (0.06s). 합계 5개 테스트 중 2개 통과, 3개 결함 재현. 언어 반복 5개는 한 UI 테스트 안에서 실행했다.
- 로그: `/tmp/deppy-fleet-user-review-ui-probes-20261001.log`, `/tmp/deppy-fleet-user-review-storage-probes-20261001.log`, `/tmp/deppy-fleet-user-review-cli-20261001.log`.
- 첫 UI probe는 egui 0.36의 Context API 이름을 잘못 사용해 컴파일되지 않았다. 격리 체크아웃의 테스트에서 `global_style_mut`로 고친 뒤 위 assertions를 실제 실행했다. 이 최초 오류는 제품 결함으로 집계하지 않았다.
- 이전 구현 단계의 전체 테스트 결과는 이번 리뷰의 재실행 결과가 아니다. 이번에는 위 재현과 UI harness를 실행했으며 전체 suite·실행 앱 E2E·메모리 실측은 다시 실행하지 않았다.
- 소스상 idle cache는 현재 세션 키로 정리되고, idle snapshot은 scalar projection과 기존 행·바이트 제한을 사용한다. 이번 범위에서 추가로 확인된 메모리 누수는 없다. 이는 프로세스 메모리 실측 결과를 뜻하지 않는다.

## 종료 상태

위 3건은 미수정 상태다. 제품 소스·0.5.0 배포 아티팩트·버전은 그대로 유지했고, 리뷰 보고서와 handoff만 갱신했다. 기존 실행 앱 PID 45213(0.4.5)은 유지했으며 재실행·커밋·푸시는 하지 않았다.
