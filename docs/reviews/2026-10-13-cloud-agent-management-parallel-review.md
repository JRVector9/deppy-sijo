| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | `crates/app/src/cloud_agent/management.rs:767` | ANSI·줄바꿈으로 나뉜 등록 비밀 값의 마스킹 누락 | 복원 가능한 비밀 값이 작업 저장·공유 경로에 남음 | 원문·정규화 결과 마스킹 및 저장 경로 회귀 검사 |
| medium | `crates/app/src/cloud_agent.rs:773` | 평상시 UI 유휴 시간을 절전 복귀로 판단 | 승인된 클라우드 입력 권한이 갑자기 회수됨 | 실제 복귀와 정상 유휴 구분 |
| medium | `crates/app/src/cloud_agent/pwa.rs:107` | 에이전트 이름·모델을 실행 상태로 전달 | 실행·대기·승인 필요·오류·종료를 구분할 수 없음 | 로컬 관측 상태와 에이전트 정보 분리 |
| medium | `web/shared/cloud-results.js:79` | 매 snapshot마다 결과 DOM 전체 교체 | 버튼 포커스와 결과 텍스트 선택 소실 | 변경 항목만 갱신하고 DOM 유지 |
| medium | `crates/app/src/cloud_agent.rs:824` | 인증 키 교체 시 삭제된 연결의 ACL 잔존 | 새 연결 권한 설정이 용량 제한으로 거절됨 | 폐기한 연결 ACL 정리 |
| medium | `crates/agent-mcp/src/oauth.rs:103` | 승인한 OAuth 요청자 이름을 잃음 | 여러 봇의 권한·취소 대상 구분 어려움 | 승인 이름을 표시 정보로 유지 |
| medium | `crates/app/src/app.rs:27187` | 원본 세션이 없으면 이동 요청을 조용히 종료 | 계속 “원래 세션을 여는 중”으로 표시됨 | 이동 결과와 실패 사유를 UI에 반환 |

# 클라우드 에이전트 관리 PR 1–6 병렬 리뷰

**후속 수정 완료(2026-10-13): R1–R7 모두 수정·검증했다.** 위 표와 아래 본문은 `fcf237f7`에서의 최초 리뷰 기록이다. 현재 결과와 실제 RED→GREEN 검사·소스 커밋은 [7건 수정 QA 보고서](../qa/2026-10-13-cloud-management-review-fixes.md)를 참조한다.

검토일: 2026-10-13. 기준 worktree는 `/Users/jr/Desktop/projects/deppy-sijo-performance`, branch는 `feat/audit-nine-pr-v0.6.0-20261004`이다. 고정 검토 범위는 **`3ac1f61f..fcf237f7`**이며, 마지막 제품 통합 커밋은 `9d0ed76b`, 검토 HEAD는 `fcf237f7d14f2c9548d653b98c1fc9e5b1aa20d8`이다. 구현 계획의 PR 1–6 전체를 대상으로 초기 PR1·웹 계층·Secret·runtime 및 최종 교차 crate 통합을 함께 검토했다. PR 번호는 구현 계획상의 작업 단위다.

독립 서브에이전트 3개 모두 **gpt-6.1-sol / xhigh / fork none**로 실행했다. Root가 근거를 재검증하고 중복을 제거했으며, 필요한 격리 재현을 실행했다. 최초 리뷰에서는 제품 코드를 수정하지 않았으며 7건을 미수정 상태로 보고했다. 이후 사용자 승인으로 7건을 수정했다.

## 검토 분담

| 범위 | 담당 | 검토 대상 |
| --- | --- | --- |
| PR1: 설정 상태·연결 안내 | review_auth_access | 준비·인증·도구 성공 구분, 초기 설정·승인 경로 |
| PR2: 연결 주체·권한·입력 제어 | review_auth_access | bearer/OAuth, principal, ACL, lease, receipt 권한, runtime 큐 입장 |
| PR3: 영속 작업·상태 | review_runs_history | CloudRun, revision, worker 저장, heartbeat, unknown, 취소 요청·확인 |
| PR4: 이력·최신성·접수 조회 | review_runs_history | history cursor, bounded output, scoped operation/receipt, Secret |
| PR5: 연결 유지·복구 | review_ui_pwa | 프로필, 재연결, 절전 간격 처리, 승인·입력 회수 |
| PR6: 결과·PWA | review_ui_pwa | native 결과·원본 이동, PWA/Relay, 관리 envelope, 정적 배포·SW |
| 교차 경계·재현·최종 판정 | Root | 실제 호출자·데이터 생산자 대조, 문서 계약, 재현, 중복 제거 |

## Details

### R1 — high: 새 작업 필드의 비밀 값 마스킹 누락

위치: [management.rs:767](../../crates/app/src/cloud_agent/management.rs#L767). `management_text`는 ESC 등 제어 문자를 먼저 제거하고 `self.redact`를 적용한다. 이 순서가 Secret의 ANSI 처리에 필요한 구분 문자를 파괴한다.

합성 등록 비밀 값 `review-private-token-2026`에 대해 `review-private-\x1b[31mtoken-2026` 원문을 직접 마스킹하면 보호된다. 현재 필터 순서를 거치면 `review-private-[31mtoken-2026`이 되고, 마스킹이 실패한다. `[31m`을 제거하면 등록된 값을 복원할 수 있다. 같은 값이 `review-private-\ntoken-2026`으로 줄바꿈되면 현재 stream matcher는 보호하지 못하지만 `redact_wrapped_text`는 보호한다.

새 `start_run`의 instruction/title과 `update_run`의 message/result/artifact label이 이 함수를 사용한다. `SaveRun`으로 저장되거나 응답·native·PWA로 전달되는 값은 이미 ANSI 경계를 잃었으며 후속 동일 stream 마스킹으로 복구할 수 없다. 저장·공유 경로는 정적으로 추적했다. 이번 재현은 실제 Secret 컴포넌트와 동일 필터 순서의 격리 검사이며, App→SQLite 전체 E2E를 새로 실행한 것은 아니다. 이전부터 존재하던 notify 경로는 이번 신규 결함 범위에서 제외했다.

개선: 원문에 complete/wrapped 마스킹을 먼저 적용하고 제어 문자 정규화 후 다시 마스킹한다. 원문 정규화가 서로 떨어진 비밀 조각을 붙일 수도 있으므로 두 단계 모두 필요하다. literal/ANSI/줄바꿈 입력을 작업 저장·재조회·native/PWA까지 검증하는 회귀 검사를 추가한다.

### R2 — medium: 정상 유휴 시간이 입력 승인 회수로 이어짐

위치: [cloud_agent.rs:773](../../crates/app/src/cloud_agent.rs#L773), [continuity.rs:43](../../crates/app/src/cloud_agent/continuity.rs#L43). `Recovery.frame`은 egui frame 사이에 30초 넘는 간격이 있으면 복귀로 판정한다. OS 절전 복귀 여부는 확인하지 않는다. 이 결과로 `take_control`이 모든 입력 grant/permit/lease를 회수한다.

정상 유휴에서도 frame 간격이 길어질 수 있다. Web/Relay는 기본 비활성이고, ResourceMonitor는 값이 변하지 않은 샘플의 repaint를 생략하며, 준비 완료한 tunnel은 주기적으로 App을 깨우지 않는다. 홈·문서 화면에 있고 PTY도 조용한 경우, 다음 마우스 동작이나 MCP 요청이 정상 입력 승인을 회수할 수 있다. 이 경로는 정적 확인이며 실제 OS 절전 또는 30초 UI 유휴 E2E는 수행하지 않았다.

개선: OS 복귀 또는 연결 liveness 증거로 재검증을 시작한다. heartbeat를 선택한다면 정상 UI 유휴와 실제 복귀를 구분할 수 있어야 한다. 정상 유휴는 승인 유지, 실제 복귀는 오래된 permit 차단을 각각 검사한다.

### R3 — medium: 로컬 상태에 에이전트 정보가 표시됨

위치: [pwa.rs:107](../../crates/app/src/cloud_agent/pwa.rs#L107), [runs.rs:149](../../crates/app/src/cloud_agent/ui/runs.rs#L149), [settings.rs:300](../../crates/app/src/cloud_agent/ui/settings.rs#L300). `local_status`에 `Target.agent_line`이 들어간다. 생산자인 [WorkspaceUi::agent_line_for:5224](../../crates/app/src/ui/workspace.rs#L5224)는 provider/model/effort 또는 에이전트 이름을 반환한다.

예를 들어 `Codex · model · effort`가 로컬 실행 상태 대신 표시된다. PWA renderer는 running/waiting/needs_approval/error/done을 기대하지만 다른 문자열은 그대로 보여준다. Native도 동일한 agent_line을 상태 레이블로 사용한다. 클라우드 보고와 로컬 승인 대기·오류·종료를 비교할 수 없다.

개선: 관측된 로컬 실행 상태와 관측 시각을 별도 필드로 전달하고 에이전트 이름·모델은 표시 정보로 유지한다. 실행·지시 대기·승인 필요·오류·종료·셸 상태를 공유 경로 전체에서 검사한다. 이번 판정은 실제 생산자와 소비자 데이터 흐름을 정적으로 확인했다.

### R4 — medium: PWA 갱신으로 포커스·텍스트 선택 소실

위치: [cloud-results.js:79](../../web/shared/cloud-results.js#L79). 모든 cloud snapshot의 accept가 render를 호출하고, render는 `list.replaceChildren()`로 전체 결과 노드를 제거한다.

App은 1초 주기로 publication을 예약한다. [pwa.rs:54](../../crates/app/src/cloud_agent/pwa.rs#L54)의 `captured_at`이 바뀌고 dashboard의 전체 frame 비교에도 포함되므로 내용이 같아도 snapshot이 전달된다. 공유 renderer를 사용하는 direct PWA와 Relay가 영향을 받는다.

격리 실제 Chrome에서 production HTML/JS/CSS를 사용했다. 제어 버튼에 실제 포커스와 결과 텍스트 선택을 설정한 후 timestamp만 바꾼 snapshot을 전달하자 `focus:false`, `connected:false`, `selection:""`이 됐다. 사용자 앱·브라우저 프로필·실제 토큰을 사용하지 않았다. 테스트 transport만 합성 fixture다.

개선: ID 기준으로 기존 DOM을 유지하고 변경된 필드만 갱신한다. 관측 시각만 달라진 경우 결과 목록을 교체하지 않는다. 포커스·텍스트 선택 유지 검사와 실제 내용 변경 검사를 추가한다.

### R5 — medium: 인증 키 교체 후 오래된 ACL로 용량 고갈

위치: [cloud_agent.rs:824](../../crates/app/src/cloud_agent.rs#L824), [control.rs:89](../../crates/app/src/cloud_agent/control.rs#L89). Auth rotation은 기존 named credential/connection을 삭제한다. App의 Rotate는 발급 lease를 비우고 `take_control()`을 호출하지만, ControlManager의 이전 named connection permission은 삭제하지 않는다. `take_control()`은 제어 lease만 회수한다.

256개 세션에 legacy 256개와 named client 8개×256개 권한을 설정한 뒤 키를 교체하고 새로운 client 8개에 권한을 설정하면, 두 번째 세대의 8번째 client에서 `client_permission_limit`으로 실패한다. 현재 연결은 8개뿐이지만 이전 연결 권한도 4096-entry 상한에 포함된다. 오래된 연결은 UI 목록에서 이미 사라져 사용자가 해당 ACL을 지울 수 없다.

현재 ControlManager를 직접 포함한 격리 Rust 검사로 재현했다. 실제 Auth/App 전체 rotation E2E는 실행하지 않았고, App rotation이 같은 manager 동작을 수행하는 것은 정적으로 확인했다.

개선: rotation으로 폐기한 named principal의 ACL을 정리한다. 기존 legacy 읽기 동의의 유효 상태를 보존하고 오래된 입력 permit은 계속 회수해야 한다. 여러 rotation을 반복해 현재 연결 수에 맞게 권한을 계속 설정할 수 있는지 검사한다.

### R6 — medium: OAuth 승인 요청자의 표시 이름을 잃음

위치: [oauth.rs:103](../../crates/agent-mcp/src/oauth.rs#L103). 승인 화면은 등록한 `Client.name`을 보여주지만 발급된 Access에는 이 표시 이름이 남지 않는다. principal은 항상 `OAuth connection`을 반환한다.

서로 다른 이름의 봇을 2개 승인하면 설정의 연결 목록에서 동일 이름으로 보인다. 목록은 불투명 UUID로 정렬되고 승인 화면은 그 UUID를 보여주지 않아 승인한 요청자와 권한 대상의 대응을 사용자가 신뢰성 있게 판단할 수 없다. 인증 주체 자체는 UUID로 분리되므로 인증 우회로 판정하지 않았다.

개선: 승인받은 client display name을 grant/principal의 표시 정보로 유지한다. 권한 기준은 계속 UUID를 사용하고 owner rename이 refresh 이후에도 보존되게 한다. 두 이름의 동시 연결·refresh·rename을 검사한다. 실제 제공자 계정 인증은 수행하지 않았다.

### R7 — medium: 닫힌 원본 세션 이동의 실패가 UI에 전달되지 않음

위치: [app.rs:27187](../../crates/app/src/app.rs#L27187), [runs.rs:126](../../crates/app/src/cloud_agent/ui/runs.rs#L126). 결과 UI는 유효한 ID가 큐에 들어가면 “원래 세션을 여는 중” notice를 설정한다. App navigation은 persisted pane이 있을 때만 동작하고 없으면 아무 결과 없이 반환한다.

원본 pane이 삭제됐으나 결과 이력은 남아 있는 경우 notice는 계속 opening 상태다. 이미 정의된 `cloud.run_navigation_failed` 및 닫힌 원본 이동 실패 문서 계약이 이 경로에서는 사용되지 않는다. pane을 찾은 이후 활성화가 실패하는 경우도 호출자에 결과를 전달해야 한다. 이번 검토는 정적 호출 경로 확인이며 실제 native 닫힌 세션 E2E는 수행하지 않았다.

개선: 요청과 원본 ID에 연결된 비동기 navigation 결과를 반환하고 missing/closed/activation failure를 UI에 표시한다. 실패하면 결과 화면을 유지한다. 성공·삭제된 원본·활성화 실패를 각각 검사한다.

## 이번 리뷰에서 실행한 검증

| 검사 | 실제 결과 | 범위·제한 |
| --- | --- | --- |
| Secret 원문 필터 순서·wrapped 입력 격리 Rust 검사 | compile exit 0; 0 PASS / 2 FAIL; exit 101 | 보호 기대를 검사해 결함 재현, 전체 App 저장 E2E 아님 |
| 현재 ControlManager의 rotation ACL 용량 격리 Rust 검사 | compile exit 0; 0 PASS / 1 FAIL; exit 101 | 같은 manager 상태 전이 재현, 전체 Auth/App rotation E2E 아님 |
| production PWA 실제 Chrome focus/selection 격리 fixture | exit 1; timestamp-only snapshot의 포커스·선택 소실 | 합성 transport, 독립 headless profile, production asset 사용 |
| `git diff --exit-code fcf237f7 -- crates web` | exit 0 | 리뷰 중 제품 소스 변경 없음 |

FAIL은 검사가 잘못된 값에 대해 성공한 것이 아니라, 보호 기대를 위반한 현재 동작을 재현한 결과다. 실제 실행 로그는 [evidence/redaction-order.log](evidence/2026-10-13-cloud-management-review/redaction-order.log), [evidence/rotation-acl.log](evidence/2026-10-13-cloud-management-review/rotation-acl.log), [evidence/pwa-focus.log](evidence/2026-10-13-cloud-management-review/pwa-focus.log)에 보존한다. 합성 비밀 값만 사용했다.

원본 격리 harness는 `/private/tmp/deppy-cloud-review-redaction-order-20261013.rs`, `/private/tmp/deppy-cloud-review-rotation-acl-20261013.rs`, `/private/tmp/deppy-cloud-review-focus-20261013.mjs`다. 브라우저 재현 명령은 `node /private/tmp/deppy-cloud-review-focus-20261013.mjs`다. 수정 작업에서는 영속 회귀 검사를 실제 crate/browser suite에 추가한다.

이전 구현 단계의 Cargo suite·strict clippy PASS는 [기존 QA 기록](../qa/2026-10-13-cloud-agent-management.md)에 있으며 이번 리뷰의 재실행 결과로 주장하지 않는다. 이번 리뷰에서는 Cargo 전체 suite·실제 클라우드 제공자 계정·공개 터널·OS sleep E2E를 실행하지 않았다.

## 수정 우선순위와 제외한 후보

1. R1을 우선 수정해 저장·조회·공유 전체의 비밀 값 보호를 검사한다.
2. 입력 안정성 R2/R5, 상태 정확성 R3를 수정한다.
3. 결과 사용성 R4/R7, 연결 식별 R6를 수정한다. 서브에이전트를 사용하면 별도 소유 파일 또는 worktree로 분리하고 교차 App 계약은 Root가 통합한다.

output byte limit의 page cursor 손실 가능성은 명시적인 truncated/lossless:false 계약과 대조 후 독립 확정 결함으로 올리지 않았다. 만료 인증 이후 opt-in 재연결의 정책 해석과 legacy 철회 상태 보존은 별도 확정 오류로 올리지 않았다. 중복 또는 불확실한 후보를 7개 발견 사항에 포함하지 않았다.

제품 코드 수정·새 제품 커밋·푸시·릴리스·앱 재실행은 이번 리뷰에 포함하지 않았다. 보고서와 handoff 및 합성 재현 로그만 변경했다.
