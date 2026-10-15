# 로컬 작업 완료를 원래 클라우드 에이전트에 자동 반환

상태: 사용자 설계 검토 대기. 제품 구현은 아직 시작하지 않았다.

## 목표와 현재 결함

연결된 클라우드 에이전트가 Deppy의 세션에 작업을 제출하면, 해당 로컬 작업의 검증된 종료를 같은 요청자에게 자동으로 돌려준다. 현재 `notify`는 클라우드의 답변을 Deppy에 저장하는 반대 방향이다. 현재 MCP 서버는 POST JSON 단발 응답만 지원하고 GET SSE는 405로 거절하므로 실제 외부 완료 전달 경로가 없다.

완료 사실, 소켓 전송, 에이전트의 수신 확인은 별개다. 연결을 버린 외부 서비스의 모델을 임의로 깨운다고 약속하지 않는다. Dot/Meta Muse/Grok Bot의 실제 계정 호환은 아직 검증되지 않았다.

## 대안과 권장안

| 방식 | 동작 | 장점과 제약 |
| --- | --- | --- |
| **권장: 원래 제출 요청에 SSE 완료 결과 반환** | 연결·세션별 동의 후 `send_text`/`paste_text`의 `submit:true`에 추적을 자동 등록하고, 그 RPC를 완료까지 유지한다. 별도 wait 도구 호출은 필요 없다. | 표준 tool result를 원 요청자가 받는다. 호스트가 장시간 요청 유지·재연결을 지원해야 한다. |
| MCP Tasks와 상태 알림 | 협상된 표준 task에 결과를 저장하고 task 상태 이벤트를 보낸다. | 장시간 작업에 적합하나 기존 지원 프로토콜에서 실험 기능이며 호스트 지원이 필요하다. |
| 제공자별 callback | 공식 제공자 API로 연결이 끝난 에이전트를 호출한다. | 해당 API·목적지·권한이 확인되어야 한다. 이번 범위에서 임의 callback URL이나 새 제공자 비밀키를 추가하지 않는다. |

## 사용자 동작

기존 환경설정 → 클라우드 에이전트 → 연결의 세션 권한에 `로컬 작업 완료 받기`를 추가한다. 읽기 공유를 전제로 하며 입력 권한을 새로 부여하지 않는다. 기본 꺼짐이고, 사용자가 연결별로 켜면 이후 적격 제출마다 자동 등록한다. 새 팝업은 추가하지 않고 기존 권한 행과 상태 컴포넌트를 재사용한다.

옵션이 꺼진 기존 14개 도구 호출은 JSON/3초 접수 응답을 유지한다. 켜진 상태의 적격 제출은 결과 반환 시점이 달라진다는 점을 설정 설명·initialize instructions·tool description에 알린다. 미지원 실행이나 추적 용량 부족은 **입력 전** 이유를 반환하며 조용히 입력만 보내고 자동 통지가 된다고 표시하지 않는다. 이미 접수 여부가 불명인 작업은 재실행하지 않는다.

## 정확한 완료 판정

1. 검증된 connection identity, 원 operation/run, workspace/session/generation, runtime·AI execution/native session identity, 현재 완료 revision을 고정한다. UI의 현재 선택 세션으로 라우팅하지 않는다.
2. durable claim 이후 입력을 전송하고 해당 operation의 실제 PTY 접수를 확인한다. `queued`는 실행·완료를 뜻하지 않는다. 접수 전 상태만으로 완료 판정을 시작하지 않는다.
3. 접수 뒤 발생한 검증된 새 TurnStart를 해당 입력에 결속한다. 기존 권한 `Grant.input_epoch`를 실제 키입력 epoch로 오인하지 않고, 별도의 입력 개입/실행 세대 증거를 사용한다. 필요한 원문 대조는 일시적인 digest로 제한하며 256byte 표시 제목으로 작업을 대조하지 않는다. digest 일치만으로 인과관계를 확정하지 않는다.
4. 같은 native turn의 검증된 Completed와 reducer의 완료 revision을 관찰한다. 기존 자식 작업·응답/승인 대기 억제 규칙을 유지한다. UI 배지 소비, 화면 정지, 단순 Done/idle, cloud progress100% 또는 `update_run`은 완료 증거가 아니다.
5. 한 대상에는 동시 tracked turn 1개만 허용한다. 중첩 제출·수동 개입·native session/execution 교체로 결속이 모호해지면 unknown으로 종료하며 다른 작업의 완료를 대신 보내지 않는다. 필요한 start/end/identity 증거가 없는 제공자는 미지원으로 명시한다.
6. 검증된 완료를 원 작업에 대한 단일 durable event로 저장한 뒤 전송한다. 상태 메타데이터만 보내며 터미널 원문·프롬프트·로컬 경로·키를 기본 payload에 넣지 않는다. 기존 cloud run의 에이전트 보고 상태는 별도로 유지한다.

## 전달·수명·권한

- 원 제출 POST를 Streamable HTTP SSE로 유지하고 같은 JSON-RPC id에 최종 tool result를 반환한다. 별도 대기 worker/슬롯을 사용하여 기존 일반 HTTP4개·actor2개 제한을 장시간 점유하지 않는다.
- authenticated transport/stream identity를 원 connection과 operation에 묶는다. 동일 credential을 재사용한 다른 호스트나 다른 stream에 방송하지 않는다. GET Last-Event-ID는 원 stream만 재개하며 입력은 재전송하지 않는다.
- stable event id를 보존해 재전달을 중복 제거한다. 네트워크 전달은 at-least-once이고 socket write는 모델 수신 확인이 아니다. 명시적 ACK가 없는 호스트는 `반환됨 · 수신 확인 없음`으로 표시한다.
- 상태: `자동 반환 대기`, `작업 중`, `완료 · 반환 대기`, `반환됨 · 수신 확인 없음`, `연결 끊김`, `미지원`, `확인 불가`, `만료`, `권한 해제`. 로컬 완료와 전달 상태를 분리한다.
- read 공유 해제·옵션 해제·연결 폐기/만료·credential 교체·서버 중지 시 pending 전송을 차단하고 stream을 닫는다. 이미 쓴 bytes를 회수한다고 주장하지 않는다. 입력 권한만 해제된 경우 유효한 읽기 권한 아래 기존 완료 receipt는 조회할 수 있다.
- 재시작 시 미완료 watch는 unknown으로 정리한다. 기존 입력·권한·새 세대 공유를 복원하지 않는다. 이미 확정된 receipt의 재전달도 원 identity에 대한 현재 읽기 권한을 요구한다.
- 초기 상한: 대상당1 tracked turn, 전역64 active watch/16 wait stream, stream당1 reader, 이벤트1KiB, 15초 heartbeat, TurnStart 결속30초, watch60분, 끊긴 stream 재개10분, 완료 receipt24시간·최대1000개. 저장/버퍼 용량 초과·만료는 명시적인 결과이며 조용히 유실하지 않는다. heartbeat가 외부 호스트 timeout을 해제한다고 가정하지 않는다. client의 요청 timeout 지원이 미확인인 경우 상태를 미확인으로 표시하고 실제 제공자 자동 수신을 보장하지 않는다.
- DB·HTTP write·재연결 대기는 UI frame 밖에서 수행한다. 모든 표시 문자열은 기존 RedactionService로 전송 직전에 정리하고 routing identity는 유지한다.

## 변경 경계와 PR 순서

1. **완료 증거/결속**: storage agent-attention에서 typed native turn/completion proof를 제공하고 app→cloud Target에 제한적으로 투영한다. operation별 watch state machine과 입력 개입 invalidation을 추가한다. 기존 세션 상태 표시 규칙은 유지한다.
2. **전송/영속성**: agent-mcp에 별도 SSE deferred lane, actor/stream scoped replay와 durable completion receipt를 추가한다. 인증·재연결·중지·용량 상한을 포함한다. 원 도구 이름과 옵션 꺼짐 동작을 유지한다.
3. **설정/통합**: 기존 연결 권한 행에 toggle과 상태를 추가하고 한국어/영어 안내·복사되는 MCP 사용 지침·가이드를 갱신한다. 실제 provider 지원을 확인되지 않은 상태에서 연결만으로 보장하지 않는다.

root가 shared app.rs/형식/문서/버전의 단일 소유자다. workers는 서로 다른 파일 경계의 patch를 제출하고 root가 교차 리뷰·통합한다. 구현 전 상세 PR 실행 계획을 작성한다.

## 검증 기준

isolated synthetic fixture와 독립 공식 MCP SDK client로 원 POST→SSE final tool result→끊김→원 stream 재개를 실행한다. 다른 actor/run/session/generation/stream, 중복 hook, 이전 완료, waiter 전 빠른 완료, 로컬에서 배지 읽음, 자식 작업·승인 대기, 입력 거절/불명, 중첩·수동 개입, 권한 폐기/만료/회전/stop, 느린 reader·용량 압력, 재시작 unknown, 늦은 비밀키 등록 마스킹을 검증한다. 기존 JSON 도구 회귀도 실행한다. 실제 외부 계정에 메시지를 보내는 검증은 이번 구현 테스트에 포함하지 않는다.

기능을 사용자용 빌드로 전달하면 마지막 shipped0.12.2보다 높은 **0.13.0**으로 올리고 binary와 macOS bundle 두 version 필드를 검증한다. 사용자 재실행 요청이 없으므로 앱은 종료·실행하지 않는다.

## 근거

- [MCP 2025-11-25 Streamable HTTP](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports): 원 POST의 SSE 최종 응답, GET 재개, stream별 이벤트 소유권.
- [MCP 2025-11-25 Tasks](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks): 선택 협상되는 task와 상태 알림.
- [공식 TypeScript SDK v1.x streaming server](https://github.com/modelcontextprotocol/typescript-sdk/blob/v1.x/src/examples/server/simpleStreamableHttp.ts): session/stream과 replay 분리 참고.
- 현재 소스: `crates/agent-mcp/src/server.rs`, `history.rs`, `management.rs`; `crates/app/src/cloud_agent.rs`; `crates/storage/src/agent_attention.rs`. worker 상세 근거 `/private/tmp/deppy-cloud-completion-design-20261015.md`.

자체 검토: 역할·상한·기존 JSON 호환·수신 의미·미지원·실패 처리를 구체화했다. 미확인 provider 호환을 구현 완료나 전달 보장으로 취급하지 않는다. 신규 기능 코드·테스트·scaffold는 승인 전 작성하지 않는다.
