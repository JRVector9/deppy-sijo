# Fleet 지시 대기 시간·다음 작업 목록 — 2026-10-01

## 결과

기존 표시는 정확한 완료 시각과 첫 화면 관측 시각을 구분하지 않았다. 알림 CAS 세대가 초 단위 시각으로 전달되는 경로도 있었다. 사진에 표시된8시간44분 자체의 정확성은 사진만으로 확정할 수 없다. 이번 수정은 실제 완료 훅의 Unix 초를 따로 저장하고, 근거가 없는 기간을 **관측 기준**으로 표시한다.

각 카드에는 **다음 작업 (개수)**와 실제 예약 프롬프트의 번호 목록을 추가했다. 기존 예약 모델은 PTY 세션별1개다. 예약이 없으면 예약된 작업 없음, 있으면 최대3줄의512자 미리보기와 전체 원문 hover를 보여준다. 편집에는 전체 원문을 유지한다. AI가 답변에 남긴 임의 TODO를 자동으로 예약하는 기능은 추가하지 않았다. 기존 다음 단계 실행/보류/취소 규칙은 유지한다.

## 시간 계산

- `agent_needs_input.idle_since`는 Unix 초이며, `attention_revision`은 계속 알림 소비/CAS 세대다. App의 `global_idle_since`는 live 세션만 읽는다.
- 완료 알림 확인은 대기 시작 시각을 지우지 않는다. 새 턴·작업·질문/승인 대기·취소·종료는 시각을 비운다. 완료 후 마지막 질문이 해결되면 실제 해결 이벤트 시각부터 다시 센다.
- `IdleObserved`만으로 완료를 추정하지 않는다. 이전 현대식 JSON에 완료 기록만 있고 시각 근거가 없을 때도 정확한 시각을 만들지 않는다.
- 업그레이드는 legacy Stop의 저장 시각과, bounded valid modern JSON의 `last_activity`가 완료 revision과 정확히 일치하고 취소/종료/미응답이 없는 경우만 복원한다. 모호한 이전 기록은 관측 기준으로 남긴다.
- 화면이 숨겨져도 기존 active/warm runtime 이벤트와 structured 상태 알림으로 이전 대기 시간을 초기화한다. idle snapshot의 완료 세대와 Unix 초는 별도다. 같은 초의 두 완료도 세대로 구분하며, 화면 감지 이벤트는 event identity가 없어 관측 기반 타이머만 초기화한다. 확인한 훅 시각은 실제 attention snapshot의 새 작업/질문/시각 소실로만 초기화한다. attention snapshot이 화면 밖에서도 시각을 갱신하므로 완료 뒤 늦게 처리된 Running 이벤트가 기록을 지우지 않는다.
- 화면 첫 관측으로 센 경우는 `관측 기준 {시간} 대기`로 구분한다. DB 시각은 초 단위이며 시스템 시간에 기반한다. 관측 시각은 과거의 실제 완료 시각을 복구한 값이 아니다.
- 재실행 시7일 정리에서도 확인한 대기 기록을 유지하되 기존 테이블 행 상한은 유지한다. idle preflight는 key/숫자 필드만 materialize하고 JSON/message를 복사하지 않는다. 부분 인덱스·행/바이트/타입/미래 시각 검증·snapshot retained-byte 상한을 적용한다. 프레임의 DB/파일 읽기와 추가 폴링은 없다.

## 독립 리뷰와 수정

| Finding | 수정 | 회귀 검증 |
| --- | --- | --- |
| 기존 modern 완료 시각이 업그레이드에서 누락 | 확인 가능한 JSON만 보수적으로 복원, 모호한 관측은 exact로 승격하지 않음 | migration 실제 SQLite, invalid JSON/request 배열도 안전히 제외 |
| Running 수신 시각보다 이른 정상 완료가 거부됨 | 이벤트 수신 시각 경계 제거·독립 완료 세대와 authoritative attention snapshot으로 구분 | 완료120, Running 수신130에서도120 채택; 이전90은 거부 |
| idle preflight가 큰 JSON을 materialize | 필요한 scalar만 투영·부분 인덱스 | bounded snapshot의 생략/바이트/잘못된 타입/미래 시각 검증 |
| 7일 startup prune이 확인한 idle 기록 삭제 | idle 보존 predicate 추가 | 오래된 acknowledged idle 유지·일반 오래된 행 삭제 |

독립 Codex CLI 첫 리뷰4 P2와 재리뷰2 P2를 모두 반영했다. Storage2/UI1, 추가 UI2 회귀 실패를 실제 실행했고 수정 후 통과했다. 재리뷰의 추가2건은 동일 초에 완료된 새 episode와, 완료 projection이 Running drain보다 먼저 도착하는 반대 순서였다. 최종 수동 코드 리뷰에서는 시각과 세대 단위가 분리되고 raw detector가 확인한 훅 기록을 초기화하지 않으며, 새 attention 상태가 숨겨진 카드 시각을 초기화함을 확인했다. 범위 내 남은 확정 결함은 없다.

## 실제 실행한 최종 검증

| 범위 | 결과 |
| --- | --- |
| App 전체 `cargo test --offline --locked -q -p deppy-sijo --bin deppy-sijo -- --test-threads=1` | 2508 passed /0 failed /28 ignored,48.62s |
| Storage 전체 | 389 passed /0 failed,10.39s |
| i18n 전체 | 8 passed /0 failed |
| Fleet 신규 focused 회귀 | Storage8 /UI8 신규 회귀 포함 전체 통과,중복 합산 안 함 |
| 실제 offscreen Fleet 카드 renderer | 1 passed,1.43s, 실행 없이 PNG 생성·직접 시각 확인 |
| App/Storage all-target Clippy | exit0,`-D warnings` |
| fmt /diff /xtask check-boundary | 모두 exit0 |
| release App +MCP proxy | exit0,23.56s |
| root +27 workspace metadata/lock | 모두0.5.0 |
| 컴파일된 reported-version marker·bundle 두 plist 값 | 모두0.5.0, 앱 실행 없이 확인 |
| 별도 Developer ID 서명 app/ZIP 로컬 패키지 검증 | exit0,압축 해제·중첩 서명·바이너리 검증 포함 |

최종 core 자동 테스트 합계2905건이며 PNG renderer1건은 별도다. 이미지 `target/fleet-waiting-next-tasks-0.5.0.png`에서 현재/마지막 작업2줄, 모델, 번호 목록, 빈 예약 상태가 카드 안에 완전히 보임을 확인했다. 배포 artifact의 GUI 사용 테스트는 수행하지 않았으며 실행 중인0.4.5 PID45213을 유지했다.

## 실패 접근

- 첫 UI RED는 Queryable trait import가 없어 컴파일되지 않았다. import 후 의도한2개 assertion RED를 실제 확인했다.
- 짧은 프롬프트가 Small 폰트에서2줄20pt였는데 테스트가20pt 초과를 요구했다. 긴3줄 프롬프트로 실제 wrapping을 검증했다. 기존 chip 기대도 새 번호 목록으로 바꿨다.
- 새6-tuple과 guard가 strict Clippy에 걸려 SavedAttention 구조와 match pattern으로 바꿨다.
- 관측 이벤트로 모호한 이전 완료 시각을 exact로 만들던 구현은 명시적 RED로 제거했다.
- 첫 seconds-only/previous-source barrier는 동시 초와 반대 이벤트 순서를 놓쳐 제거했다. 완료 세대를 별도로 투영하고 실제 attention 상태를 authoritative하게 적용하며 raw 이벤트는 관측치에만 반영한다. 실제 DB same-second 세대/ack 회귀와 UI 두 순서·source 소실 회귀를 추가했다.
- 첫 패키지 서명은 helper 이름을 cloudflared로 잘못 기대해 중간 assertion으로 멈췄다. 실제 deppy-cloudflared를 확인하고 helpers→main→bundle 순서로 서명·ZIP 재작성 후 verifier가 통과했다. 실패한 stage를 배포/실행하지 않았다.

## 버전·소스

- 0.4.11 → **0.5.0** (다음 작업 목록 기능 추가, 대기 시간 오류 수정).
- base commit `166f8daeb1054cf09a07194fa83bf0a4a19d93ce` +기존 popup 수정 및 이번 수정의 uncommitted product SHA256 `59ba2623add7fbf42ecc4e4c33910553965e790b54ee2d3a2220c0c9969d13a1`.
- 산출물: `target/bundle-0.5.0/Deppy Sijo.app`, `target/bundle-0.5.0/Deppy Sijo.zip`.
- 이 검증은 로컬 개발 패키지 검사이며 notarization 주장에 해당하지 않는다. 커밋/푸시·앱 종료/재실행은 하지 않았다. 원래 deppy-sijo worktree는 수정하지 않았다.
