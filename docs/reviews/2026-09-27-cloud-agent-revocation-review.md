| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | crates/runtime/src/in_process.rs:2145 | 입력 큐를 처리할 때 회수된 권한을 확인하지 않음 | 입력 제어 회수 뒤에도 기존 큐의 텍스트가 PTY에 전달됨 | 워커 입장 시 권한 유효성 검사·취소 장치 |
| medium | crates/app/src/cloud_agent.rs:453 | claim 직후 만료 시 결과를 확정하지 않음 | 입력 없음 응답과 영구 unknown 기록이 엇갈림 | no-effect 결과를 내구성 있게 기록 |
| medium | crates/agent-mcp/src/oauth.rs:271 | 긴 리다이렉트 응답 전에 승인을 소모함 | 응답 유실과 재시도 실패 | 인코딩 후 길이 검증 |
| medium | crates/agent-mcp/src/oauth.rs:384 | refresh scope를 문자열로 비교함 | 정상 권한 순서 변경·범위 축소가 거부됨 | 권한 집합과 승인 범위 검증 |

# 클라우드 에이전트 권한 회수·만료 추가 코드 리뷰

> 후속 상태: 네 결함은 `36e987c9`, `37c808c1`에서 모두 수정했다. [최종 수정·검증 결과](2026-09-27-cloud-agent-four-fixes.md)를 현재 상태로 참조한다. 아래 내용은 당시 미수정 코드의 리뷰 기록이다.

날짜: 2026-09-27

## 범위와 결과

- 사용자 요청: 다시 코드 리뷰. 제품 수정 없이 검토와 근거 확인을 수행했다.
- 실제 소스: `/Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp`, `feat/cloud-agent-mcp`, HEAD `0d28fb1a`. 직전 리뷰 이후 제품 소스 변경은 없다.
- Codex CLI는 `2a1583d1..HEAD`의 클라우드 에이전트 관련 소스와 호출 경로를 검토했다. 문서·계획·일지는 입력에서 제외했다. 권한 회수, 만료, 상관 ID 수신증, 세션 세대, 동시 HTTP와 내구성 있는 재시도 처리를 중심으로 검토했다. 알려진 OAuth 두 건은 새 결함으로 재보고하지 않도록 지정했다.
- 독립 CLI 리뷰에서 **신규 high 한 건, medium 한 건**이 나왔다. 직접 소스 확인과 production 코드 기반 임시 재현 프로그램으로 둘 다 확인했다. 기존 OAuth medium 두 건도 기존 실제 Server HTTP 재현 프로그램을 다시 실행해 유지됨을 확인했다.
- 총 미해결 항목은 **high 1 / medium 3**이다. Critical은 이번 검토에서 확인하지 못했다.

## Details

### 신규 high: 입력 제어 회수가 워커 큐의 입력을 차단하지 않음

- `cloud_agent.rs:453`은 App에서 요청의 토큰/기한을 검사하고, App `app.rs:23935`는 `WriteInputTracked { operation_id, session, bytes }`를 runtime 큐에 보낸다.
- `take_control`(`cloud_agent.rs:199`)은 App의 grant.input만 false로 바꾼다. 세션 공유 해제·listener stop·token rotate도 이미 전송한 runtime 명령에 취소 정보를 전달하지 않는다.
- 실제 PTY 입장 단계인 `in_process.rs:2145`는 grant/epoch/deadline 확인 없이 `admit_input`을 호출한다. 명령에는 권한/취소 식별자가 없다.
- 따라서 runtime 큐 처리가 늦어질 때 App의 권한 검사 → 큐 적재 → 사용자 회수 → 워커 처리 순서가 가능하며, 권한 회수 뒤 텍스트가 PTY에 전달된다. Settings의 take-control 주석은 클릭 이후 입장이 차단된다는 의미를 명시하고 있지만, 현재 검사는 runtime 명령 큐 입장 시점에만 있다.
- 실제 재현은 public runtime wake callback에 테스트용 gate를 설치해 워커의 dequeuing을 잠시 멈췄다. `CloudAgent::handle`의 전송 closure는 **실제** `host.send_command(WriteInputTracked)`에 성공했다. `take_control` 후 새 요청은 거부됨을 확인하고 gate를 열었다. 기존 요청은 실제 `InputAdmitted(Ok)`로 완료되고 임시 셸 터미널에서 텍스트가 관찰됐다.
- 결과: `new_request=rejected, old_admission="queued", terminal_output=true`.
- 이 fixture는 사용자 GUI나 사용자 에이전트에 입력하지 않았다. 임시 일반 셸과 로그 폴더를 사용했으며 자체 AI 에이전트나 LLM 요청을 시작하지 않았다. gate는 지연 순서를 결정하기 위한 테스트 장치다. 자연스러운 부하 상태에서 같은 타이밍을 별도로 측정했다고 주장하지 않는다.
- 수정 방향: 아직 PTY에 입장하지 않은 cloud input에 회수 가능한 권한/취소 정보를 붙이고, 실제 워커 입장 직전에 검사한다. runtime 큐와 App 사이의 취소/회수 계약이 필요하다. 이미 PTY에 전달된 바이트를 회수할 수 있다는 의미가 아니다.

### 신규 medium: claim 후 만료 분기가 영구 unknown 기록을 남김

- `History::claim`은 operation row를 `{"status":"unknown","retry":false}`로 저장하고 commit한다.
- `cloud_agent.rs:453`의 후속 liveness 검사는 SQLite의 최대 100ms 대기 동안 기한/토큰이 만료된 경우 `expired_or_revoked_request_no_effect`를 반환한다. 그러나 이미 저장한 row에 `finish`를 호출하지 않는다.
- 같은 ID와 인자로 다시 요청하면 `Claim::Existing` 분기가 영구 unknown을 반환한다. 실제 효과가 없는 경로라고 응답했는데 저장된 감사 기록과 수신증은 이를 나타내지 못한다.
- 실제 재현은 production History DB에 별도 SQLite connection의 `BEGIN EXCLUSIVE`를 걸고 80ms 후 풀었다. 요청은 진입 시 유효한 25ms 기한이었다. claim은 성공했지만 후속 liveness 검사에서 만료됐다. 전송 closure가 호출되지 않음을 확인했다.
- 최초 결과: `Err("expired_or_revoked_request_no_effect")`. 유효한 새 Request로 동일 operation ID를 재조회한 결과: `{"retry":false,"status":"unknown"}`. 재조회에서도 전송 closure는 호출되지 않았다.
- 수정 방향: 효과를 수행하지 않은 것으로 확정된 분기는 그 결과를 durable receipt로 완료한다. 완료 기록 자체가 실패했을 때의 보수적인 unknown 처리는 별도로 유지한다. 기존 ID를 지워 무조건 재실행하는 방식은 at-most-once 보장을 손상시킬 수 있다.

### 기존 OAuth medium 두 건

직전 [상세 리뷰](2026-09-27-cloud-agent-mcp-rereview.md)의 두 문제는 소스가 미수정 상태다. 실제 Server HTTP probe를 다시 실행한 결과도 동일했다.

```text
long redirect: redirect_bytes=2048, state_bytes=2048, first_status=0, response_bytes=0, retry_status=400, retry_error="authorization_expired"
refresh scope="deppy.input deppy.read": status=400, error="invalid_scope"
refresh scope="deppy.read": status=400, error="invalid_scope"
refresh scope="": status=200, error=null
```

## 실제 실행한 검증과 제한

- CLI 리뷰: `/tmp/deppy-cloud-revoke-review-20260927.log`, exit0, 신규 P1/P2 두 건. CLI에는 테스트 실행을 요청하지 않았다.
- 신규 fixture: `/tmp/deppy-cloud-revoke-probe/main.rs`. CloudAgent와 Settings leaf는 현재 production 파일을 수정 없이 복사했고, 사용하지 않는 화면의 Answer DTO/시간 표시 helper만 작은 fixture stub으로 제공했다. MCP/runtime/SQLite/terminal/secret은 기존 App test build와 정확히 일치하는 compiled rlib를 사용했다. 제품 로직을 재작성한 가짜 구현이 아니다.
- 첫 rustc 시도는 최신 mtime으로 고른 서로 다른 Cargo feature graph의 rlib와 빠진 UI helper 때문에 실패했다. 제품 빌드 실패가 아니다. App test fingerprint에 기록된 정확한 dependency fingerprint로 선택하고 fixture helper를 제공한 뒤 컴파일 exit0이었다. 로그 `/tmp/deppy-cloud-revoke-probe-build{,-fixed}.log`.
- 신규 재현 실행 `/tmp/deppy-cloud-revoke-probe/run`: exit0, 두 결함의 예상 assertion 성공. 로그 `/tmp/deppy-cloud-revoke-probe-20260927.log`.
- 기존 OAuth 재현 `/tmp/deppy-cloud-repeat-probe`: 이번에도 exit0, 위 오류 결과 재현.
- 기존 단위 테스트 19 MCP +10 App의 성공은 **직전 리뷰에서 실행한 결과**이며 이번에 다시 실행하지 않았다. 제품 소스는 변하지 않았고 이번에는 미포함된 경계 사례의 재현에 집중했다. 전체 workspace 또는 외부 Grok Bot 성공을 주장하지 않는다.
- 제품 소스·제품 테스트 수정, 커밋, release 빌드, Deppy GUI 실행/재실행, 공개 터널, push는 수행하지 않았다. 보고서와 handoff만 갱신했다.

## 남은 작업과 다음 명령

리뷰 요청은 완료했다. 네 결함 모두 제품 수정은 아직 적용하지 않았다. 실제 Grok Bot 계정/공개 HTTPS 연결은 별도 미검증 상태다. 기존 warm 세션에 허용된 입력을 전달하는 기능은 있으나, 검토한 브리지 자체에서 새 AI 에이전트를 자동 실행하는 동작은 확인하지 못했다.

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp
git status --short
cat docs/reviews/2026-09-27-cloud-agent-revocation-review.md
cat /tmp/deppy-cloud-revoke-probe/main.rs
cat /tmp/deppy-cloud-revoke-probe-20260927.log
sed -n '442,480p' crates/app/src/cloud_agent.rs
sed -n '2142,2156p' crates/runtime/src/in_process.rs
```

다음 수정 단계에서는 위 실제 큐/SQLite 경계 조건을 회귀 테스트로 추가하고, 기존 MCP/App/runtime 검증을 실행한다. 사용자 허락 없이 앱을 재실행하지 않는다.
