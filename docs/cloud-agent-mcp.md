# 클라우드 에이전트 MCP 연결

## Deppy에서 준비하기

1. **설정 → 클라우드 에이전트**를 엽니다.
2. 기본 로컬 포트는 `8739`입니다. 모바일 웹 기능과 독립된 포트입니다.
3. 기본 선택인 **자동 주소 생성**을 사용합니다. 배포 앱에 포함된 연결 도우미가 임시 HTTPS 주소를 만들므로 별도 터널 설치나 계정이 필요하지 않습니다. 도우미가 없는 개발 빌드는 고정 주소를 직접 연결하거나 도우미가 포함된 앱을 사용합니다.
4. **MCP 서버 켜기**를 누릅니다. 서버는 `127.0.0.1`에서만 수신합니다. **로컬 MCP 서버 준비**와 **공개 MCP 주소 확인됨**은 서버·주소 준비 상태이며 봇의 인증이나 도구 호출 성공을 뜻하지 않습니다.
5. 원하는 세션에 **읽기 · 답변 수신**을 켭니다. 다른 워크스페이스의 세션도 개별 선택할 수 있습니다.
6. **주소 복사**로 봇의 MCP 커넥터에 `/mcp` 주소를 등록합니다. OAuth 커넥터는 Mac의 이 설정 화면에서 승인합니다. Bearer 커넥터는 **토큰 복사**를 사용합니다. 입력이 필요하면 그 세션의 **입력 허용**도 켭니다.
7. 봇에게 `list_sessions`를 호출하도록 요청하고 선택한 세션이 반환되는지 확인합니다. URL 등록, 인증, 도구 호출은 공개 주소 준비와 별개의 단계입니다. 초기 확인을 위해 터미널에 시험 명령을 보낼 필요는 없습니다.

포트·공개 호스트·주소 방식만 설정에 저장합니다. 서버, 토큰, 공유·입력 권한은 앱 재시작 시 자동으로 켜지지 않습니다. 토큰은 24시간 뒤 만료되고, 연결 끄기나 토큰 재발급으로 즉시 폐기됩니다. 재발급하면 입력 권한도 해제됩니다.

## HTTPS 터널

공개 URL이 있어야 클라우드에서 로컬 Mac으로 접속할 수 있습니다. 기본 자동 모드는 포함된 도우미로 Cloudflare Quick Tunnel을 관리합니다. 임시 주소는 다시 연결하면 바뀔 수 있으므로 커넥터 URL도 갱신해야 합니다. Quick Tunnel은 가동 시간을 보장하지 않고 SSE를 지원하지 않습니다. 현재 Deppy의 POST JSON 응답에는 SSE가 필요하지 않습니다. [Cloudflare Quick Tunnel 안내](https://developers.cloudflare.com/tunnel/get-started/quick-tunnels/)

같은 주소를 계속 사용하려면 **고정 주소 직접 연결**을 선택합니다. `deppy.example.com` 또는 `mac.example.ts.net:8443`처럼 **호스트와 선택적 포트만** 입력하고 아래 방법 중 하나로 HTTPS 터널을 별도로 구성합니다. 이 모드에서 서버 시작은 로컬 수신만 확인합니다. 공개 터널의 도달 여부와 실제 커넥터 등록은 사용자가 확인해야 합니다. 고정 주소만으로 Mac의 잠자기·앱 종료를 해결하지는 않습니다.

### Cloudflare named Tunnel

기존 Cloudflare Tunnel에 전용 호스트를 연결합니다. ingress 예:

```yaml
ingress:
  - hostname: deppy.example.com
    service: http://127.0.0.1:8739
  - service: http_status:404
```

Deppy의 공개 호스트는 `deppy.example.com`, 커넥터 주소는 `https://deppy.example.com/mcp`입니다. 브라우저 로그인 페이지를 요구하는 Cloudflare Access 정책은 봇의 Bearer 인증과 별도로 호환되는지 확인해야 합니다. [공식 Tunnel 안내](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/)

Named Tunnel의 공개 호스트와 Deppy에 입력한 호스트를 일치시킵니다. 이 구현은 POST JSON 응답을 사용하며 SSE를 요구하지 않습니다.

### Tailscale Funnel

기존 모바일 웹 Serve 설정을 바꾸지 않도록 비어 있는 HTTPS 포트를 선택합니다. 예:

```sh
tailscale funnel --bg --https=8443 http://127.0.0.1:8739
```

CLI가 출력한 주소가 `https://mac.example.ts.net:8443`이면 Deppy의 공개 호스트는 `mac.example.ts.net:8443`, 커넥터 주소는 `https://mac.example.ts.net:8443/mcp`입니다. Funnel은 인터넷 공개 접속이고 Serve는 tailnet 내부 접속입니다. 계정의 Funnel 허용 정책과 Mac용 지원 설치 형태를 확인하세요. [공식 Funnel 안내](https://tailscale.com/docs/features/tailscale-funnel)

## OAuth 커넥터 승인

MCP 주소는 `/mcp`입니다. 커넥터는 401의 `WWW-Authenticate`에서 보호 리소스 메타데이터를 발견하고, 공개 클라이언트를 등록한 뒤 authorization-code + S256 PKCE를 사용합니다. `resource`는 공개 HTTPS MCP 주소와 정확히 같아야 합니다.

1. 커넥터에서 MCP 주소를 등록하고 인증을 시작합니다.
2. 브라우저의 **Approve in Deppy** 화면을 둡니다.
3. Mac의 **설정 → 클라우드 에이전트**에서 요청한 이름, 리다이렉트 주소, 읽기/입력 범위를 확인한 뒤 승인 또는 거부합니다. 이름은 클라이언트가 등록한 값입니다. 주소도 확인하세요.
4. 승인하면 브라우저가 원래 커넥터로 돌아갑니다. 입력 범위를 승인해도 세션별 **입력 허용**은 별도로 필요합니다.

액세스 토큰은 최대 1시간, 회전하는 리프레시 토큰은 현재 연결의 최대 24시간 안에서만 유효합니다. 연결 끄기·토큰 재발급·앱 종료로 모두 폐기됩니다. 승인하지 않은 등록은 5분 뒤 만료되며, 승인 코드는 60초간 한 번만 교환할 수 있습니다. 인증 정보는 메모리에만 있고 토큰 원문은 기록하지 않습니다.

갱신할 때 권한 순서를 바꿔도 같은 권한으로 처리합니다. 입력 권한을 가진 연결은 액세스 토큰을 읽기 전용으로 줄일 수 있으며, 회전된 리프레시 토큰에는 원래 승인 범위가 유지됩니다. 원래 승인하지 않은 입력 권한을 추가할 수는 없습니다. 리다이렉트 주소와 `state`의 최종 인코딩이 응답 헤더 한도를 넘으면 승인 요청을 만들기 전에 거부합니다.

지원 흐름은 공개 클라이언트 동적 등록(DCR), authorization-code/S256, refresh-token, 정확한 redirect/resource 결합입니다. Client ID Metadata Documents, 기밀 클라이언트의 client-secret 인증, 브라우저 CORS 클라이언트는 지원하지 않습니다. [MCP 인증 규격](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization)

## 커넥터 등록과 에이전트 자체 답변 받기

Bearer 헤더를 지원하는 MCP 커넥터에 URL을 등록하고 인증을 다음 형태로 설정합니다.

```text
Authorization: Bearer <Deppy에서 복사한 토큰>
```

Grok 웹의 Custom MCP 등록 경로는 [공식 커넥터 안내](https://docs.x.ai/grok/connectors)를 참고하세요. Grok 웹의 지원 여부와 사용 중인 Grok Bot의 커넥터 기능은 따로 확인해야 합니다. OAuth 커넥터는 아래 승인 흐름을 사용합니다. 실제 Grok Bot 계정 등록은 해당 계정과 공개 터널에서 별도로 확인해야 합니다.

Deppy의 **에이전트 작업 지시문 복사**를 눌러 봇의 지시문/스킬에 붙여넣습니다. 복사문은 `send_text`와 `paste_text` 선택, UTF-8 바이트 한도, 동일 작업 수신증 처리, 원래 세션으로의 `notify` 반환을 함께 안내합니다. 핵심 지시:

> 세션 목록에서 선택한 session_id와 generation을 유지한다. 작업 또는 분석을 마칠 때, 터미널 명령을 보내지 않았더라도 자신의 최종 답변 전체를 notify 도구로 Deppy에 보낸다.

예를 들어 로컬 프로젝트 작업 중 관련 리모트 셸 세션을 공유하고 봇에게 “이 세션의 오류를 분석해서 답변을 Deppy로 보내줘”라고 요청합니다.

1. 봇이 `list_sessions`로 사용자가 공유한 세션을 찾습니다.
2. `read_output`으로 화면을 읽습니다.
3. 입력 허용이 켜져 있으면 단일 줄 명령을 `send_text`로, 여러 줄 프롬프트는 확인된 bracketed paste 세션에 `paste_text`로 전달합니다.
4. **봇 자신의 분석 결과를 `notify`로 보냅니다.** 로컬 에이전트의 출력만 읽고 끝내지 않습니다.
5. Deppy의 기존 알림에 답변 도착이 표시됩니다. 원래 터미널 pane의 접을 수 있는 답변 영역에 표시되며, 알림을 누르면 원래 세션으로 이동해 답변을 펼칩니다. 원래 세션이 닫혔으면 설정의 답변 기록으로 이동합니다. 이후에는 자유롭게 스크롤하거나 답변을 접을 수 있습니다. 설정의 **답변 · 입력 기록**에서 전체 답변을 읽거나 복사할 수 있습니다.

다른 탭에서 작업하더라도 답변은 원래 세션에 연결됩니다. 입력 제어를 회수하거나 연결을 끊어도 이미 받은 답변 기록은 남습니다. 터미널의 TUI 화면이나 stdin에 봇의 설명을 주입하지 않습니다.

Grok bot과 Cloud Agent 모두 같은 흐름을 사용합니다. [명시적 붙여넣기와 원래 세션의 자체 답변 안내](cloud-agent-mcp-guide.md)에 JSON 예, 모드·초안 보호, 크기 제한과 `unknown` 처리 방법이 있습니다.

봇이 대화에서 답변만 하고 `notify`를 호출하지 않으면 Deppy로 전달되지 않습니다. 일반 채팅 답변을 가로채는 기능은 아닙니다. 웹훅으로 봇을 깨우는 기능도 포함하지 않습니다.

## 도구 계약

| 도구 | 용도 | 주요 입력 |
| --- | --- | --- |
| `list_sessions` | 공유한 세션 목록 | 없음 |
| `read_output` | 변경된 터미널 화면 읽기 | `session_id`, `generation`, 선택적 `cursor` |
| `send_text` | 텍스트 입력·선택적 Enter | 위 식별자, `operation_id`, `text`, `submit` |
| `paste_text` | 명시적 여러 줄 붙여넣기·선택적 Enter | 위 식별자, `operation_id`, `text`, `submit` |
| `send_ctrl_c` | Ctrl+C 한 번 보내기 | 위 식별자, `operation_id` |
| `notify` | 봇 자체 답변 저장·알림 | 위 식별자, `operation_id`, `message` |

- `submit` 기본값은 `false`입니다. `send_text`는 개행·탭·ESC·제어문자를 거부합니다. `paste_text`는 확인된 DEC 2004 모드에서 LF·CRLF·탭을 허용하며, 단독 CR·ESC·나머지 제어문자는 거부합니다. Enter는 `submit=true`, Ctrl+C는 별도 도구로만 보냅니다.
- `send_text`는 최대 8 KiB, `paste_text`는 최대 32 KiB, 답변은 최대 16 KiB의 UTF-8 바이트입니다. paste의 직렬화한 arguments와 전체 HTTP 요청은 각각 64 KiB 제한이 있어 JSON 이스케이프나 봉투 때문에 먼저 거부될 수 있습니다. 등록된 비밀은 화면과 수신 답변에서 가립니다.
- `list_sessions`는 `paste_bracketed`, `paste_text_max_bytes`, `paste_ai_confirmed`도 반환합니다. 실제 붙여넣기에서는 원래 세션·실행·권한·인증·마감 시간을 재검증하고, bracketed paste가 필요한 본문은 실제 런타임 큐 입장 시 모드가 꺼져 있어도 거부합니다. 본문과 선택적 Enter는 하나의 큐 예약입니다. AI에 새 프롬프트를 제출할 때 기존 초안·대화 상자를 보호하며, 의도적인 no-submit append는 기존 초안에 붙일 수 있습니다.
- `queued`와 `admission:pty_queue`는 해당 PTY 입력 큐가 수락했다는 뜻입니다. 워커가 거부하면 `rejected`를 저장합니다. 셸의 실제 실행·완료는 보증하지 않습니다. 출력으로 작업 결과를 확인하세요.
- **입력 제어 회수**, 입력 허용 해제, 공유 해제, 연결 종료·토큰 재발급은 아직 PTY에 전달되지 않은 입력도 차단합니다. 다시 입력을 허용해도 이전 큐의 입력이 살아나지 않습니다. 이미 PTY가 수락한 입력은 회수할 수 없습니다.
- 동일 작업 재요청에는 동일 `operation_id`를 사용하세요. 다른 내용을 같은 ID로 보내면 거부됩니다. 전달 여부가 `unknown`이면 새 ID로 자동 재실행하지 마세요.
- 처리 전 만료되어 입력하지 않은 작업은 `rejected`로 기록합니다. 동일 ID로 다시 조회해도 같은 결과를 반환합니다.
- 화면 읽기는 최신 가시 화면 스냅샷입니다. 모든 stdout 바이트를 빠짐없이 전달하는 로그 스트림은 아닙니다. 커서가 달라지면 새 화면을 반환하고, `reset=true`이면 화면을 다시 동기화하세요. 응답은 최신 관측 캐시이며 `may_be_stale=true`와 `refresh_requested=true`를 명시합니다. `screen:null`은 캐시가 동일하다는 뜻일 뿐 새 출력이 없다는 확인이 아닙니다. `retry_after_ms` 뒤 다시 읽으면 비동기 갱신 화면을 받을 수 있습니다.
- 읽을 때 기존 15초 원격 화면 표시 임대를 갱신하므로 숨겨진 warm 워크스페이스도 화면을 갱신합니다. 탭 전환을 강요하지 않습니다.
- 세션 종료·새 런타임 복원으로 generation이 바뀌면 입력을 거부하고 공유 권한을 다시 받아야 합니다.

## 로컬 저장과 재연결

앱 DB 폴더의 `cloud_agent_history.db`에 작업 수신증과 답변을 저장합니다. 최근 입력·작업 기록 500건과 최근 완료된 답변 100개를 각각 보존해 화면에 표시합니다. 원문 입력이나 토큰은 저장하지 않고 입력 바이트 수·Enter 여부·결과만 기록합니다. 중복 실행 방지를 위해 최대 100,000개의 요청 ID 수신증을 보존하고, 상한에 도달하면 새 작업을 거부합니다. 답변 본문은 100개의 더 새로운 답변이 완료된 뒤 비웁니다. 입력 기록이 늘어나거나 미완료 요청이 접수돼도 답변을 밀어내지 않습니다.

Mac이나 Deppy가 꺼져 있으면 연결은 실패합니다. 봇은 나중에 다시 연결할 수 있으나 새 토큰 등록과 공유 선택이 필요합니다. Mac 종료 중 보낸 답변을 중계 서버에 보관하지 않습니다.

## 제공자 호환성과 실제 계정 검증

2026-10-13 기준으로 **아래 제공자의 실제 계정과 Deppy 연결의 호환성은 모두 미검증**입니다. 공식 기능 설명, 로컬 자동화 테스트, 실제 제공자 계정 연결 증거를 구분합니다. 서비스 이름은 OpenAI dot, Meta Muse, Grok Bot으로 해석했습니다. 동명 서비스는 별도 확인이 필요합니다.

| 제공자 | 공식 자료에서 확인한 범위 | 실제 계정 연결 검증 상태 | 실제 계정에서 확인할 항목 |
| --- | --- | --- | --- |
| OpenAI dot | 클라우드 컴퓨터·연결 앱·작업 사용 | 미검증 · 임의 Deppy MCP 등록 경로 미확인 | 커넥터 등록, 인증 방식, 권한, 갱신, `notify` 반환 |
| Meta Muse | 자체 VM·브라우저·연결 앱·장기 작업 | 미검증 · 임의 Deppy MCP 등록 경로 미확인 | 공개 MCP 등록 가능 여부, 인증 정책, 도구 호출, 결과 반환 |
| Grok Bot | MCP 서버·플러그인·스킬 지원 | 미검증 · Deppy 실제 계정 테스트 미실행 | URL 등록, OAuth 또는 Bearer, 갱신, multiline, `notify`, 재연결 |
| Grok 웹 Custom MCP | 공개 MCP URL 등록과 인증 흐름 | 미검증 · Grok Bot과 별도 제품 | 해당 계정의 인증 방식, 여섯 도구 탐색, 호출·답변 저장 |

공식 자료: [OpenAI dot 안내](https://help.openai.com/en/articles/20001530-getting-started-with-your-dot), [Meta Muse 발표](https://about.fb.com/news/2026/09/introducing-muse-personal-ai-agent/), [Grok Bot 가이드](https://x.ai/bot/guides/grok-bot-101), [Grok 웹 커넥터 안내](https://docs.x.ai/grok/connectors). 이 자료만으로 Deppy OAuth·도구 호출·응답 반환의 호환성을 보장하지 않습니다.

실제 계정 연결 검증 기록에는 제공자·제품·계정 플랜(비밀 값 제외), 검증 날짜·앱 버전, URL 등록·인증·`list_sessions`·`read_output`·승인된 입력·`notify`·만료/갱신·재연결 각각의 결과를 남깁니다. 현재 완료된 연결 검증 기록은 없습니다.

## 자동화 검증 범위

```sh
cargo test -p agent-mcp
cargo test -p deppy-sijo --bin deppy-sijo cloud_agent
cargo test -p deppy-sijo --bin deppy-sijo ui::notifications
cargo test -p i18n
cargo build -p deppy-sijo --release
```

저장소의 테스트는 로컬 OAuth/HTTP→App→테스트 PTY 입력 수락→실행 출력→테스트 에이전트 자체 답변, 권한·중복 수신·보존·egui 표시를 대상으로 합니다. 이 문서의 명령 목록은 실행 결과 보고가 아닙니다. 해당 변경의 실제 실행 결과는 프로젝트 handoff에 기록합니다. 실제 제공자 계정 등록·인터넷 터널·봇의 도구 호출 준수 여부는 위 표의 별도 검증 대상입니다.

runtime wire 버전은22입니다. paste의 모드 입장 조건은 로컬 전용이며 wire 버전을 추가 변경하지 않습니다. 별도 runtime 피어를 사용한다면 같은 버전으로 빌드해야 하며, 이전 버전은 handshake에서 거부됩니다. SSH 터미널 세션은 기존 로컬 runtime의 PTY를 계속 사용합니다.
