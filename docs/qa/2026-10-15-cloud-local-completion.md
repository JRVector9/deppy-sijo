# 클라우드 원 요청으로 로컬 작업 완료 반환

승인된 설계: [native proof와 원 요청의 완료 반환](../superpowers/specs/2026-10-15-cloud-local-completion-design.md). 버전0.12.3에서0.13.0으로 신규 기능을 추가한다. PR235를 main에 반영하고0.13.0 로컬 개발용 패키지 검증을 완료했다. 앱을 종료하거나 실행하지 않았다.

## 사용하는 방법

1. 환경설정의 클라우드 에이전트 → **접근 권한**에서 연결을 선택한다.
2. 해당 세션의 조회를 허용하고 **로컬 작업 완료 받기**를 켠다. 기본은 꺼짐이다. 이 옵션은 입력 권한이나 제어권을 새로 주지 않는다.
3. 클라우드 호스트가 새 initialized MCP session에서 `send_text` 또는 `paste_text`를 `submit:true`로 요청한다. 기존 입력 권한과 유효한 제어권도 필요하다.
4. 지원되는 로컬 Codex 실행의 새 native turn이 검증되면 같은 JSON-RPC 요청에 완료 결과가 반환된다. 별도 대기·조회 도구 호출은 필요 없다.
5. 로컬 완료와 완료 반환 상태를 따로 확인한다. `반환됨 · 수신 확인 없음`은 에이전트 모델이 읽었다는 확인이 아니다.

새 기능은 검증 가능한 Codex native hook·실행 신원·입력 접수 순서·완료 rollout을 요구한다. 해당 실행에서 이미 관찰한 genuine native proof와 accepted-input sequence가 사전 기준으로 필요하다. 처음 만든 세션이나 복원 직후 아직 이 기준이 없는 세션은 첫 추적 제출을 입력 전에 거절할 수 있다. 필요한 증거가 없는 제공자도 입력 전에 미지원으로 거절한다. UI 제목, idle 배지, 작업 진행률100%, `update_run`은 완료 근거로 쓰지 않는다. 기능이 꺼진 기존14개 도구의 JSON/접수 응답은 유지한다.

기본 자동 주소의 Cloudflare Quick Tunnel은 SSE를 지원하지 않으므로 이 선택 기능에 사용할 수 없다. SSE가 가능한 별도 고정 터널이 필요하다. [Cloudflare 공식 제한](https://developers.cloudflare.com/tunnel/get-started/quick-tunnels/)(2026-10-15 확인).

Dot/Meta Muse/Grok Bot 등 실제 클라우드 호스트의 요청 시한·재연결 지원은 미확인이다. 호스트가 장시간 요청을 유지하거나 원 stream을 재개해야 한다. 공식 SDK 합성 검증을 실제 제공자 계정 호환으로 확대하지 않는다.

## 변경과 검토

- 실제 PTY accepted input sequence와 full-prompt digest를 native turn·실행 세대·원 connection/operation/run/session/generation에 결속한다. 같은 워크스페이스·같은 에이전트라도 다른 세션의 완료는 사용할 수 없다.
- 입력 전 bounded SSE 자리와 durable claim을 확보한다. 전용 transport lane이 일반HTTP4개/actor2개 슬롯을 장시간 점유하지 않는다. 입력을 재시도하지 않고 원 MCP session/stream만 재개한다.
- 공개 operation에는 입력 접수 결과만 유지하고, 최종 완료 receipt는 완전한 원 transport identity로 제한한다. DB 기록 뒤 원 요청에 단일 결과를 반환한다.
- runtime event 유실은 Unknown으로 확정하고 대기 중인 입력 fence를 닫는다. 읽기 권한이 유효하면 Unknown 결과 자체는 원 요청으로 반환한다. 다른 runtime의 장애가 정상 runtime을 막지 않는다.
- 수동 개입·권한 해제·세션/실행 교체·절대 만료·늦은 admission·과대한 응답·history capacity·Closed delivery 경합을 회귀 검사했다. active watch는 UI 이력 정리로 취소되지 않는다.
- 작업자는 GPT-6.1-Sol/xhigh로 proof/runtime, MCP/ledger, 설정/UI를 별도 worktree에서 병렬 구현했다. root가 실제 RED를 실행하고 최소 수정, 교차 리뷰, 최종 통합을 담당했다. 세 경계가 최종 App 연결에서 상호 의존하므로 불완전한 중간 상태를 main에 배포하지 않고 기능 전체를 원자적 PR로 통합한다.

## 실제 실행 결과

- 최종 집중 검사: App76PASS, agent-mcp18PASS/1ignored, 실패0. `cargo test --offline --locked --no-fail-fast -p deppy-sijo -p agent-mcp completion_ -- --test-threads=1 --nocapture`.
- 영향 경로 검사(stage10): storage10PASS+integration2PASS, runtime3PASS, mcp-proxy parser1PASS+실제subprocess1PASS. App의 당시2실패는 input/transport permit 분리 뒤 오래된 테스트 조건을 바꿔 위 최종 집중 검사에서 재검증했다. 단계별 검사를 전체 합계에 중복 합산하지 않는다.
- 공식 MCP SDK1.32.1: 원 POST 최종 응답, 명시 시한을 둔61초 대기, 강제 연결끊김 후 원 stream GET 재개, 입력 재전송0/별도조회0. 최신source0.13.0 quick검증도3요청/3완료/강제끊김1/오류0. 두 실행의 cohort와 한계는 [SDK 재현 자료](2026-10-15-cloud-completion-sdk.md)에 따로 기록한다.
- source793b74f6 전체 workspace 실제5814PASS/0FAIL/65ignored,78harness. strict workspace all-target Clippy·fmt·경계·deps 모두exit0, 같은 head GitHub8checks 모두PASS.
- 이후 MCP idle delivery publication의 wake 누락을 실제 HTTP/SSE 테스트로 재현(0PASS1FAIL)하고 기존 callback을 lock 밖에서 호출하는 최소 수정으로 같은 테스트1PASS를 확인했다. 최종 제품sourcec7ba7f94(문서만 추가된 PRheadfa2c2489)에서 MCP전체92PASS1ignored, App집중76PASS, strict workspace all-target Clippy·fmt·경계·deps 모두exit0. 공식 SDK 재빌드 후 quick3요청3완료/오류0(정상52ms·750ms설정지연782ms·끊김재개1033ms), 입력재전송/별도조회0. 이전 전체결과와 중복 합산하지 않는다.
- PR235 finalheadfa2c2489 GitHub8checks 모두SUCCESS. CI nextest679+5136=5815최종PASS. 기존PTY kill test1개가 첫시도 `process group identity changed`로 실패한 뒤 설정된 재시도에서 통과했다. 같은 source의 별도 local5회는 모두 첫시도PASS였으며 원인미확정으로 기록한다. PTY source/검사정책은 바꾸지 않았다.
- main0b54b888에 merge완료. 같은 제품tree로 release/package 실제exit0(96.766s). CLI `Deppy Sijo 0.13.0`, bundle `CFBundleShortVersionString`·`CFBundleVersion` 모두0.13.0, arm64, deep/strict서명·archive검증 모두PASS. `target/bundle-0.13.0/Deppy Sijo.app` 및ZIP에 보존했고 원본package와복사본hash일치, 기존0.12.3산출물hash불변도확인했다. Apple공증 배포가 아닌 로컬개발용 package다. 상세 source cohort와 원로그는 [검증 증거](evidence/2026-10-15-cloud-local-completion/)에 보존한다.

## 보존할 경계

읽기 공유·옵션·연결·credential·서버가 폐기되면 새 전송을 막는다. 입력 권한만 철회하면 이미 접수된 작업의 유효한 읽기 결과는 유지한다. 재시작은 미완료 watch를 Unknown으로 정리하며 입력·권한·전송을 자동 복원하지 않는다. 사용자 DB·실계정·키·clipboard와 실행 중인 앱은 이 검증에서 조작하지 않았다.
