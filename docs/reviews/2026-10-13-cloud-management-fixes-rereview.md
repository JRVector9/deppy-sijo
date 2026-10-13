| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | `cloud_agent/management.rs:674` | 비밀 키의 늦은 등록 후 캐시를 다시 마스킹하지 않음 | Native/PWA에서 등록 비밀 노출 | 표시·공유 시 현재 키로 wrapped 재마스킹 |
| medium | `cloud_agent/continuity.rs:152` | 샘플 실패가 절전 이전 시계 기준을 삭제 | 복귀 후 오래된 입력 승인이 유지될 수 있음 | 마지막 정상 기준 보존 또는 불확실 상태에서 승인 회수 |
| medium | `web-remote/assets/app.js:1799` | PWA 원본 이동 실패·만료를 조용히 폐기 | 사용자가 이동 실패를 알 수 없음 | 요청별 실패 안내 및 App 결과 전달 |

# 7건 수정 후 2차 독립 코드 리뷰

2026-10-13. 검토 범위 **`fcf237f7..a3843d99`**, 현재 HEAD **`bf13eb48`**, canonical worktree `/Users/jr/Desktop/projects/deppy-sijo-performance`. 사용자 “한번 더 코드리뷰해” 요청으로 7건 수정 전체와 관련 생산자·소비자·권한·캐시·복원 경로를 다시 대조했다. 독립 담당 3명 모두 **gpt-6.1-sol / xhigh / fork none**로 실행했다. Root가 후보를 재검증하고 중복 제거했다.

**추가 확인 3건, 모두 미수정.** 제품 소스는 변경하지 않았다. 기존 QA의 통과는 그때 실행한 범위에 대한 결과이며 이번에 확인한 경계를 검증하지 않는다. N1/N2는 수정 후에도 남은 보호 경계, N3는 기존에도 있던 PWA 실패 통지 누락이다. 새로 생긴 회귀와 기존 미완결을 구분한다.

**후속 해결 완료:** 위 내용은 리뷰 당시의 상태다. 이후 사용자 “코드 수정해” 요청으로 N1–N3을 모두 수정했다. 소스 커밋은 N2 `5fff2264`, N1 `8c5e0db4`, N3 `7e02a46d`이며, [최신 QA와 회귀 증거](../qa/2026-10-13-cloud-residual-fixes.md)에 실제 실행 결과와 추가 보호 범위를 기록했다. 원래 RED 증거는 그대로 보존한다.

## Details

### N1 — high: 키가 늦게 등록되면 작업 캐시·PWA에서 비밀 노출

위치: [management.rs:674](../../crates/app/src/cloud_agent/management.rs#L674), [management.rs:633](../../crates/app/src/cloud_agent/management.rs#L633), [pwa.rs:157](../../crates/app/src/cloud_agent/pwa.rs#L157). 신뢰도 10/10.

`install_cloud_run`은 읽기/저장 결과가 캐시에 들어올 때만 `scrub_cloud_run`을 호출한다. 이후 `cloud_runs()`는 캐시를 그대로 반환한다. 초기 이력을 먼저 읽고 다른 워크스페이스 복원이나 에이전트 실행으로 해당 키의 실행 lease가 나중 등록되면, 캐시 삽입 시 없었던 마스킹 패턴이 새로 추가된다. 이때 캐시는 갱신되지 않는다.

Native 결과 화면은 캐시의 title/message/result를 직접 표시한다. PWA는 `self.redact`를 다시 적용하지만 stream 방식이라 줄바꿈으로 나뉜 비밀 값은 보호하지 못한다. 새로 등록된 키의 원문을 현재 `redact_wrapped_text`가 보호할 수 있는데도, cache와 snapshot은 보호하지 못한다.

정상 호출 근거: App의 새 RedactionService 생성 → cloud history bootstrap → runtime `resolve_secret_set`/`acquire_dotenv_redaction_lease`의 후속 등록. 제품은 모든 credential을 startup에 미리 `SeedRedaction`하지 않는다. 기존 legacy 회귀는 키를 bootstrap **이전**에 등록하므로 이 순서를 놓친다.

실제 격리 App 검사: HEAD `bf13eb48`의 detached worktree에서 production `setup`/SQLite HistoryWorker/CloudRun/bootstrap/cloud_snapshot을 호출하는 검사만 추가했다. 구현은 수정·복제하지 않았다. 먼저 wrapped legacy run을 저장·load하고 그 다음 합성 비밀 lease를 등록했다. 현재 서비스가 원문을 `[REDACTED]`로 바꿀 수 있음을 먼저 확인했다. 이후 native cache/title/message/result snapshot 모두 `masked=false`였다. **0 PASS / 1 FAIL, exit 101**, compile 성공, 검사 0.02초. 따라서 heartbeat의 60초 stale 재설치와 무관한 즉시 노출이다.

수정 방향: 표시·복사·publication 시 현재 corpus로 complete/wrapped 마스킹을 적용하거나, corpus 버전 변경 시 캐시를 다시 처리한다. 키가 load 이전/이후에 등록되는 두 순서와 native/PWA를 함께 검증한다. artifact URI는 기존 exact-safe 또는 제외 규칙을 유지한다.

증거: [late-secret-red.log](evidence/2026-10-13-cloud-fixes-rereview/late-secret-red.log), [격리 테스트 diff](evidence/2026-10-13-cloud-fixes-rereview/late-secret-test.patch).

### N2 — medium: clock 샘플 실패가 절전 감지 기준을 지움

위치: [continuity.rs:152](../../crates/app/src/cloud_agent/continuity.rs#L152), [continuity.rs:105](../../crates/app/src/cloud_agent/continuity.rs#L105). 신뢰도 10/10(상태 전이 재현, 실제 OS sleep 미실행).

`bounded_sample`은 두 awake clock 읽기 사이 scheduler 지연이 100ms를 넘으면 `None`을 반환한다. `Recovery.frame(None)`은 마지막 정상 sample을 지운다. 복귀 직후 첫 샘플이 실패하면 다음 정상 샘플을 새 기준으로 삼아 절전을 놓친다.

현재 소스를 직접 포함한 격리 Rust 검사에서 `pre(continuous=100, awake=50) → None → post(140,51)`을 적용했다. 절전 39초의 누적 차이가 있는데도 복귀 결과가 false였다. 보호 기대는 **0 PASS / 1 FAIL, exit 101**로 확인했다. Root도 동일 보호 검사를 다시 실행해 exit 101을 확인했다.

App은 `observe_continuity`에서 복귀를 감지해야 `take_control`을 호출한다. 이 호출이 빠지면 기존 grant와 아직 유효한 120초 named lease가 남는다. 복귀 **이후 새 요청**은 새로운 acceptance sample을 캡처하므로 post-vs-post 입력 검사를 통과할 수 있다. 절전 **이전 queued request**를 차단하는 기존 회귀는 이 경로를 검증하지 않는다. 이 App 호출 경로는 정적으로 대조했다.

수정 방향: 일시적인 샘플 실패에는 마지막 정상 기준을 보존해 다음 정상 샘플과 비교한다. 샘플이 계속 불확실한 상태에서 새 입력을 허용하는 정책도 명시해야 한다. 불확실 상태에서 승인을 회수하는 방법을 선택하면 정상 idle 동의를 불필요하게 잃지 않는 검사도 필요하다. 현재 `unavailable_or_backward_clocks_reset_the_baseline_without_idle_revalidation`은 기준 소실을 기대하므로 수정해야 한다.

증거: [clock-baseline.log](evidence/2026-10-13-cloud-fixes-rereview/clock-baseline.log), [Root 재실행](evidence/2026-10-13-cloud-fixes-rereview/clock-baseline-root.log), [현재 소스 include harness](evidence/2026-10-13-cloud-fixes-rereview/clock-baseline.rs).

### N3 — medium: PWA에서 원본 세션 이동 실패를 표시하지 않음

위치: [app.js:1799](../../crates/web-remote/assets/app.js#L1799), [app.js:1887](../../crates/web-remote/assets/app.js#L1887), [cloud-results.js:204](../../web/shared/cloud-results.js#L204). 신뢰도 10/10. **기존 baseline에서도 동일한 잔여 결함이며 새로운 회귀는 아니다.**

PWA는 `cloud_control_result.status=queued`를 받으면 원본 시청 준비를 시작한다. 이 응답은 의도대로 접수만 뜻한다. App이 나중에 계산한 원본 삭제·활성화 실패 결과는 native navigation 상태에만 기록되고 PWA로 전달되지 않는다. 클라이언트도 종료·30초 만료 시 `cancelPendingWatch()`만 호출하여 실패 안내를 만들지 않는다.

사용자는 준비 안내를 잠깐 보고, 이후 일반 연결 상태만 보게 된다. 결과 이력은 유지되지만 왜 이동하지 않았는지 알 수 없다. shared DOM 유지 수정은 이 실패 통지 경로를 해결하지 않는다.

제품 index/app.js/shared viewer-core/cloud-results와 독립 Chrome profile을 사용하는 합성 transport fixture를 current와 `fcf237f7`에 각각 실행했다. 닫힌 원본 dashboard 및 31초 앞으로 이동한 Date.now 만료 경로에서 모두 `failureShown=false`, `watch=[]`였다. 준비 notice는 6.1초 뒤 숨겨지고 새 snapshot은 feedback을 일반 연결 문구로 덮어쓴다. 두 실행은 exit 0이지만 이는 **결함 행동 재현 성공**이며 보호 검사 PASS가 아니다. browser errors는 없었다.

수정 방향: cloud navigation 전용 request를 유지하고 닫힘·만료·App 활성화 실패를 해당 요청에 연결해 안내한다. 일반 notification deep link의 조용한 폐기 정책과 구분한다. 늦은 결과가 새 이동 요청을 덮어쓰지 않는 회귀도 필요하다.

증거: [current browser 로그](evidence/2026-10-13-cloud-fixes-rereview/navigation-current.log), [baseline browser 로그](evidence/2026-10-13-cloud-fixes-rereview/navigation-baseline.log), [제품 자산 fixture](evidence/2026-10-13-cloud-fixes-rereview/navigation-proof.mjs).

## 이번 실제 실행과 범위

| 검사 | 결과 | 제한 |
| --- | --- | --- |
| `node crates/web-remote/tests/fixtures/run-cloud-results-mobile.mjs` | **CLOUD_RESULTS_MOBILE_OK:50**, exit 0 | 기존 회귀 재실행, N1/N2/N3 경계는 포함하지 않음 |
| 격리 detached worktree의 App late-registration 검사 | compile 성공, **0 PASS/1 FAIL**, exit 101 | 실제 SQLite/cache/DTO; 실제 사용자 앱 UI 미실행 |
| 현재 continuity.rs include Rust proof | compile exit 0, **0 PASS/1 FAIL**, exit 101 | 합성 clock 상태 전이, 실제 OS sleep/App E2E 미실행 |
| 동일 clock proof Root 재실행 | **0 PASS/1 FAIL**, exit 101 | 동일 경계 재확인 |
| PWA navigation current/baseline 실제 Chrome proof | 각 exit 0, 결함 행동 동일 | 합성 transport/시각, 실제 App wire E2E 미실행 |
| `git diff --exit-code bf13eb48 -- crates web` | exit 0 | canonical 제품 소스 유지 |

R3 상태·DTO, R4 DOM 권한/선택, R5 rotation/legacy ACL, R6 OAuth 이름/refresh/owner rename, R7 native 이동의 추가 확정 신규 회귀는 찾지 못했다. clock admission 이후 임의 instruction 사이의 sleep 가능성은 실질적인 blocking 근거가 부족해 finding에서 제외했다. 스타일·추측성 문제나 이미 검증·수정된 문제를 재보고하지 않았다.

실제 사용자 DB/키/클립보드/브라우저 프로필을 사용하지 않았다. 앱 재실행·제품 수정·push·release 없음. 이번 문서와 handoff 및 격리 증거만 canonical에서 변경했다. 원래 dirty deppy-sijo와 기존 HTML을 보존했다.

재현 명령: N1은 `/private/tmp/deppy-cloud-rereview-late-secret-20261013`에서 `CARGO_TARGET_DIR=/Users/jr/Desktop/projects/deppy-sijo-performance/target CARGO_BUILD_JOBS=2 cargo test --locked -p deppy-sijo --bin deppy-sijo rereview_late_secret_registration -- --nocapture`. N2의 정확한 rustc/실행 명령은 clock-baseline.log에 보관했다. N3는 `node /private/tmp/deppy-cloud-navigation-rereview-20261013.mjs current` 및 동일 명령의 `fcf237f7` 인자다. 영속 fixture 사본의 경로는 원 canonical을 가리키며 합성 transport만 사용한다.
