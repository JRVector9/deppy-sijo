# 수정·검증 결과 — 2026-09-27

커밋: `c9d4fbb2`, `57c0bb80` (`feat/cloud-agent-mcp`). 아래 최초 리뷰는 수정 전 `1edfb957`의 기록이다.

| 항목 | 수정 | 검증 |
| --- | --- | --- |
| PTY 수락 결과 누락 | 작업 ID가 있는 입력 명령/수락·거부 이벤트, durable 수신증, 불확실한 결과는 재입력 금지 | 실제 PTY 수락·QueueFull·없는 세션, 잘못된 runtime ACK 무시·중복·늦은 ACK |
| 원래 터미널 답변 누락 | persistent UUID에 답변 pane 투영, 펼치기/복사, 알림에서 원래 세션 이동·닫혔으면 기록 | egui 표시·세션 격리·복사, 알림 UUID 보존, 실제 HTTP 답변 수신 |
| 입력 증가 시 답변 삭제 | 최신 완료 답변 100개와 최신 audit 500개 독립, 기존 DB 마이그레이션 | 500개 입력·100개 미완료 알림·역순 완료·기존 DB 정리 |
| Bearer 인증만 지원 | OAuth resource/server 발견, DCR, S256 PKCE, 로컬 소유자 승인, refresh 회전, 범위/만료/폐기 | OAuth→HTTP→App→실제 셸 실행 출력→자체 답변 전체 왕복 |
| 압박 테스트의 입장 실패 | 거부된 typed runtime Backpressure만 재시도; 수락된 입력 재시도 없음 | 수정 후 20회 연속 및 최종 재검증 통과 |

## 최종 검증

- `cargo test -p agent-mcp`: **19 passed** (`/tmp/deppy-cloud-final-mcp-capacity-fixed.log`). HTTP 소켓 수정 뒤 당시 17개 MCP 전체 테스트를 10회 연속 실행해 모두 통과.
- `cargo test -p deppy-sijo --bin deppy-sijo cloud_agent`: **10 passed**, 실제 OAuth/HTTP/PTY 왕복 포함 (`/tmp/deppy-cloud-final-app.log`).
- pane 표시/복사 2, 알림 navigation 1, runtime tracked 3, command 20, event 7, protocol 2, 이전 peer hello 1, 기존 pressure 1, i18n 8 통과. 중복 실행/겹치는 필터가 있으므로 단순 합을 전체 고유 테스트 수로 주장하지 않는다.
- check-boundary/check-deps, cargo fmt --all -- --check, git diff --check 통과. 전체 워크스페이스 테스트를 실행했다는 주장은 하지 않는다.
- 최종 `cargo build -p deppy-sijo --release`: **성공** 21.30초 (`/tmp/deppy-cloud-five-fixes-release.log`). 앱 실행/재실행 없음.
- 소스 CLI 리뷰에서 2 P1 + 8 P2를 모두 반영했다. 마지막 token issuance 재리뷰에 추가 결함 없음 (`/tmp/deppy-cloud-oauth-capacity-review.log`). 문서/계획/일지는 리뷰에서 제외했다.

## 추가로 수정한 문제와 실패 기록

- 간헐적 HTTP400/ConnectionReset: 독립 accept probe에서 macOS 비차단 상속을 확인(100ms read timeout에도 1.2µs WouldBlock). 분할 요청 회귀가400을 재현했다. accepted socket을 blocking으로 바꾸고 전체 read/write deadline은 유지했다. RED `/tmp/deppy-cloud-fragmented-red.log`; 반복 로그 `/tmp/deppy-cloud-metadata-repeat{,-fixed}.log`.
- UI에서 MCP Record 직접 참조로 경계 검사 실패 → 순수 Answer view model로 교체. allowlist 예외 추가 없음.
- OAuth 승인 주소 표시, 미사용 등록 제한/만료, 코드·거부 redirect grace, pending 등록 만료 panic, token capacity 및 발급 실패 시 기존 grant 보존을 회귀 테스트와 함께 보완했다.
- 운영 세션을 생성하거나 에이전트를 자동 실행하는 기능 경로는 추가하지 않았다. 테스트만 임시 셸 PTY를 만들고 종료했다. 실제 입력은 사용자가 공유하고 입력을 허용한 기존 세션에 전달된다. notify는 PTY stdin에 넣지 않는다.

## 남은 외부 확인

- 실제 Grok Bot 계정·공개 HTTPS 터널은 제공되지 않아 확인하지 않았다. 로컬 표준 OAuth 클라이언트 검증과 구분한다. 봇은 자신의 답변을 `notify`로 호출해야 하며 일반 채팅 답변을 자동으로 가로채지 않는다.
- 원격 runtime protocol은 v18이다. 구버전 peer는 handshake에서 거부되므로 동일 버전 워커를 사용해야 한다.
- 지원 범위: DCR 공개 클라이언트/authorization-code/S256/refresh. Client ID Metadata Documents, client-secret, 브라우저 CORS 클라이언트는 미지원이다.

---

# 최초 리뷰 기록 (수정 전)

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | crates/app/src/cloud_agent.rs:358 | PTY 거부 결과가 MCP 수신증에 반영되지 않음 | 입력 유실을 확인·안전하게 재요청할 수 없음 | 작업 ID를 가진 PTY 수락·거부 응답 연결 |
| high | crates/app/src/app.rs:23945 | 자체 답변은 알림·설정에만 표시됨 | 원래 터미널 화면에서 답변을 볼 수 없음 | 원래 세션에 연결된 답변 표시 UI 추가 |
| medium | crates/agent-mcp/src/history.rs:111 | 본문 삭제 후 답변 알림이 남음 | 알림을 눌러도 답변을 열 수 없음 | 답변 보존과 알림 만료 정책 연동 |
| medium | crates/agent-mcp/src/server.rs:258 | 수동 Bearer만 지원, 실제 Bot 연결 미검증 | Grok Bot·OAuth 전용 커넥터 호환성 보장 불가 | 실제 Bot 등록·왕복 확인, 필요 시 OAuth 지원 |
| low | crates/runtime/src/in_process.rs:9656 | 기존 입력 압박 테스트 간헐 실패 | 압박 상황 검증을 안정적으로 반복할 수 없음 | 큐 수락과 압박 관측의 동기화 보완 |

# Cloud agent MCP review — 2026-09-27

## Scope

- Worktree: `/Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp`.
- Branch/HEAD: `feat/cloud-agent-mcp`, `1edfb957`; base `2a1583d1`.
- Source and official Grok/MCP guide review only. No production source fixes, app restart, tunnel publication or GitHub publication.
- Independent source-only `codex review`: `/tmp/deppy-cloud-agent-review-20260927.log`. It identified PTY admission (P1) and stale answer notification links (P2). Other rows above distinguish a user-experience gap, compatibility limits and an existing flaky test.

## Details

### 1. PTY admission is not confirmed

`send_text`/`send_ctrl_c` route to the existing target runtime with `WriteInput` (`app.rs:23926`). `RuntimeCommandSink::send_command` accepts the runtime command queue (`runtime/src/in_process.rs:396`), after which `active.write_input` can reject or backpressure the payload (`in_process.rs:2113`). The MCP receipt remains `queued` and an exact retry returns that receipt rather than retrying a known failed PTY admission.

The current tool contract honestly describes `queued` as runtime admission and `completion:not_confirmed`. This review does not claim the code promises shell completion. The defect/functional gap is that the later PTY outcome never reaches the operation receipt, leaving the caller unable to distinguish delivered input from dropped input. A tracked PTY admission result should preserve at-most-once behavior; execution completion is a separate concern.

The HTTP-to-App fixture (`cloud_agent.rs:759`) records effect bytes with a closure. It does not exercise `App::pump_cloud_agent` and the actual PTY together. Existing real-PTY tests separately verify hidden/warm routing and snapshot delivery.

### 2. Cloud answers are not rendered in the original terminal surface

`notify` persists the answer before ACK (`cloud_agent.rs:370`) and emits an `AnswerNotice`. App posts a notification (`app.rs:23945`); clicking it opens CloudAgents Settings (`app.rs:33501`), whose history renders the full answer (`cloud_agent/ui.rs:165`). There is no terminal pane answer renderer or original-session focus action on this path.

This was a deliberate reduced-plan choice, not an unexpected code branch. It does not satisfy an expectation of seeing Grok's own answer directly beside the ongoing terminal work. The next change should render a session-bound answer in the existing workspace/pane while keeping ordinary answer prose out of PTY stdin.

### 3. Live notifications can point to removed answers

`History::finish` clears message bodies outside the latest 500 operations, counting input operations as well as answers. `recent()` loads only those 500 records. Notifications are a separate 100-item collection and input operations do not evict cloud-answer notifications. Therefore a single answer can still have a clickable notification after 500 subsequent input actions, but its record is missing from the view.

Reproduced against the actual compiled `agent_mcp::History` API with an in-memory SQLite database: store one answer, finish 500 input actions, assert the answer is absent from `recent()` while its stored idempotency receipt remains. Probe source: `/tmp/deppy-cloud-history-probe-20260927.rs`. This was not a production-user database probe.

### 4. Guide compliance is partial and provider compatibility is not certified

JSON POST responses, valid notification ACK 202, GET 405 when SSE is absent, Origin validation and loopback binding match the basic [MCP Streamable HTTP transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports). SSE is optional here.

The [xAI API remote-MCP guide](https://docs.x.ai/developers/tools/remote-mcp) supports custom authorization headers. This makes manual Bearer authentication a plausible API integration path, but does not establish that the user's Grok Bot connector accepts the same registration flow. [Grok web custom connectors](https://docs.x.ai/grok/connectors) and [Grok Bot Marketplace plugins](https://docs.x.ai/grok-bot/computer-and-apps) describe different product flows.

The implementation has no OAuth discovery, refresh or account sign-in flow. The [MCP authorization specification](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization) recommends its OAuth-based flow for HTTP implementations that support authorization. The intentionally reduced token-only implementation should not be presented as universal cloud-agent compatibility. A real public HTTPS connector→Deppy→PTY→output→notify roundtrip remains untested.

`read_output` returns the latest observed visible-screen cache, explicitly `may_be_stale:true`, after requesting an asynchronous refresh. It cannot guarantee a fresh post-command screen or retain every intermediate stdout line. This is the documented snapshot contract, not a lossless output-stream implementation.

### 5. Existing input-pressure test is intermittent

`input_pressure_events_are_coalesced_per_session` failed at `in_process.rs:9656` with runtime command queue backpressure, then passed on one rerun. The file has no diff against the feature base. This is evidence of an existing test synchronization/admission issue, not evidence that the feature causes a regression.

Logs: `/tmp/deppy-cloud-input-pressure-review-20260927.log` and `/tmp/deppy-cloud-input-pressure-review-retry-20260927.log`.

## Background execution

- No `SpawnAgent`, `RespawnArchivedAgent`, agent process launch or LLM generation call was found in the feature's production paths.
- It runs an MCP listener and bounded request-handler threads when the user turns the connection on.
- Authorized input targets an existing session even if its tab/workspace is hidden. Existing terminal processes can continue there; the UI does not automatically switch tabs. A command explicitly sent to the shell can itself start a process, as normal terminal input can.
- `read_output` grants a 15-second viewport lease. It refreshes an existing terminal screen; it does not launch an agent.
- Grok's own cloud execution/routines are separate, controlled in Grok. Deppy neither starts those routines nor stops an already running process merely by taking back future input permission.

## Verification actually executed

| Command | Result |
| --- | --- |
| `cargo test -p agent-mcp` | 9 passed |
| `cargo test -p deppy-sijo --bin deppy-sijo cloud_agent` | 8 passed |
| `cargo test -p runtime --lib 원격_시청 -- --test-threads=1` | 2 passed |
| `cargo test -p runtime --lib input_pressure_events_are_coalesced_per_session` | Initial run failed; retry 1 passed |
| Actual `History` API retention probe | Missing answer after 500 inputs reproduced |

20 unique existing tests passed across the final runs, with the single intermittent initial failure reported above. No full-workspace PASS or real provider connection is claimed. No app rebuild/restart was necessary for this read-only source review.

## Recommended next implementation order

1. Operation-correlated PTY admission result and real MCP→App→PTY/output tests.
2. Original-session answer display and navigation.
3. Retention/notification consistency.
4. Actual Grok Bot registration and public tunnel roundtrip before claiming compatibility.
