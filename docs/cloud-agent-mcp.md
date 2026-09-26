# 클라우드 에이전트 MCP 연결

## Deppy에서 준비하기

1. **설정 → 클라우드 에이전트**를 엽니다.
2. 기본 로컬 포트는 `8739`입니다. 모바일 웹 기능과 독립된 포트입니다.
3. 공개 터널의 HTTPS 호스트를 입력합니다. `deppy.example.com` 또는 `mac.example.ts.net:8443`처럼 **호스트와 선택적 포트만** 입력합니다.
4. **MCP 연결 켜기**를 누릅니다. 서버는 `127.0.0.1`에서만 수신합니다.
5. 원하는 세션에 **읽기 · 답변 수신**을 켭니다. 다른 워크스페이스의 세션도 개별 선택할 수 있습니다.
6. **주소 복사**, **토큰 복사**로 커넥터를 등록합니다. 입력이 필요하면 그 세션의 **입력 허용**도 켭니다.

포트와 공개 호스트만 설정에 저장합니다. 서버, 토큰, 공유·입력 권한은 앱 재시작 시 자동으로 켜지지 않습니다. 토큰은 24시간 뒤 만료되고, 연결 끄기나 토큰 재발급으로 즉시 폐기됩니다. 재발급하면 입력 권한도 해제됩니다.

## HTTPS 터널

공개 URL이 있어야 클라우드에서 로컬 Mac으로 접속할 수 있습니다. Deppy는 터널이나 클라우드 서버를 자동으로 설치·배포하지 않습니다. 아래 둘 중 하나만 구성하세요.

### Cloudflare named Tunnel

기존 Cloudflare Tunnel에 전용 호스트를 연결합니다. ingress 예:

```yaml
ingress:
  - hostname: deppy.example.com
    service: http://127.0.0.1:8739
  - service: http_status:404
```

Deppy의 공개 호스트는 `deppy.example.com`, 커넥터 주소는 `https://deppy.example.com/mcp`입니다. 브라우저 로그인 페이지를 요구하는 Cloudflare Access 정책은 봇의 Bearer 인증과 별도로 호환되는지 확인해야 합니다. [공식 Tunnel 안내](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/)

개발용 Quick Tunnel은 URL이 바뀌므로 반복 사용에는 named Tunnel을 권장합니다. 이 구현은 POST JSON 응답을 사용하며 SSE를 요구하지 않습니다.

### Tailscale Funnel

기존 모바일 웹 Serve 설정을 바꾸지 않도록 비어 있는 HTTPS 포트를 선택합니다. 예:

```sh
tailscale funnel --bg --https=8443 http://127.0.0.1:8739
```

CLI가 출력한 주소가 `https://mac.example.ts.net:8443`이면 Deppy의 공개 호스트는 `mac.example.ts.net:8443`, 커넥터 주소는 `https://mac.example.ts.net:8443/mcp`입니다. Funnel은 인터넷 공개 접속이고 Serve는 tailnet 내부 접속입니다. 계정의 Funnel 허용 정책과 Mac용 지원 설치 형태를 확인하세요. [공식 Funnel 안내](https://tailscale.com/docs/features/tailscale-funnel)

## 그록봇 등록과 자체 답변 받기

Bearer 헤더를 지원하는 MCP 커넥터에 URL을 등록하고 인증을 다음 형태로 설정합니다.

```text
Authorization: Bearer <Deppy에서 복사한 토큰>
```

Grok 웹의 Custom MCP 등록 경로는 [공식 커넥터 안내](https://docs.x.ai/grok/connectors)를 참고하세요. Grok 웹의 지원 여부와 사용 중인 Grok Bot의 커넥터 기능은 따로 확인해야 합니다. OAuth만 받는 커넥터는 이 1차 Bearer 방식과 바로 연결되지 않습니다.

Deppy의 **그록봇 작업 지시문 복사**를 눌러 봇의 지시문/스킬에 붙여넣습니다. 핵심 지시:

> 세션 목록에서 선택한 session_id와 generation을 유지한다. 작업 또는 분석을 마칠 때, 터미널 명령을 보내지 않았더라도 자신의 최종 답변 전체를 notify 도구로 Deppy에 보낸다.

예를 들어 로컬 프로젝트 작업 중 관련 리모트 셸 세션을 공유하고 봇에게 “이 세션의 오류를 분석해서 답변을 Deppy로 보내줘”라고 요청합니다.

1. 봇이 `list_sessions`로 사용자가 공유한 세션을 찾습니다.
2. `read_output`으로 화면을 읽습니다.
3. 입력 허용이 켜져 있으면 필요한 명령을 `send_text`로 전달합니다.
4. **봇 자신의 분석 결과를 `notify`로 보냅니다.** 로컬 에이전트의 출력만 읽고 끝내지 않습니다.
5. Deppy의 기존 알림에 답변 도착이 표시됩니다. 알림을 누르면 원래 세션에 연결된 답변 기록을 펼치고 해당 위치로 스크롤합니다. 이후에는 자유롭게 스크롤하거나 답변을 접을 수 있습니다. 설정의 **답변 · 입력 기록**에서 전체 답변을 읽거나 복사할 수 있습니다.

다른 탭에서 작업하더라도 답변은 원래 세션에 연결됩니다. 입력 제어를 회수하거나 연결을 끊어도 이미 받은 답변 기록은 남습니다. 터미널의 TUI 화면이나 stdin에 봇의 설명을 주입하지 않습니다.

봇이 대화에서 답변만 하고 `notify`를 호출하지 않으면 Deppy로 전달되지 않습니다. 일반 채팅 답변을 가로채는 기능은 아닙니다. 웹훅으로 봇을 깨우는 기능도 포함하지 않습니다.

## 도구 계약

| 도구 | 용도 | 주요 입력 |
| --- | --- | --- |
| `list_sessions` | 공유한 세션 목록 | 없음 |
| `read_output` | 변경된 터미널 화면 읽기 | `session_id`, `generation`, 선택적 `cursor` |
| `send_text` | 텍스트 입력·선택적 Enter | 위 식별자, `operation_id`, `text`, `submit` |
| `send_ctrl_c` | Ctrl+C 한 번 보내기 | 위 식별자, `operation_id` |
| `notify` | 봇 자체 답변 저장·알림 | 위 식별자, `operation_id`, `message` |

- `submit` 기본값은 `false`입니다. CR/LF·ESC·제어문자가 든 텍스트는 거부합니다. Enter는 `submit=true`, Ctrl+C는 별도 도구로만 보냅니다.
- 입력은 최대 8 KiB, 답변은 최대 16 KiB의 UTF-8 바이트입니다. 등록된 비밀은 화면과 수신 답변에서 가립니다.
- `queued`는 런타임 명령 큐에 등록됐다는 뜻입니다. PTY 입력 수락이나 명령 완료를 보증하지 않습니다. 출력으로 작업 결과를 확인하세요.
- 동일 작업 재요청에는 동일 `operation_id`를 사용하세요. 다른 내용을 같은 ID로 보내면 거부됩니다. 전달 여부가 `unknown`이면 새 ID로 자동 재실행하지 마세요.
- 화면 읽기는 최신 가시 화면 스냅샷입니다. 모든 stdout 바이트를 빠짐없이 전달하는 로그 스트림은 아닙니다. 커서가 달라지면 새 화면을 반환하고, `reset=true`이면 화면을 다시 동기화하세요. 응답은 최신 관측 캐시이며 `may_be_stale=true`와 `refresh_requested=true`를 명시합니다. `screen:null`은 캐시가 동일하다는 뜻일 뿐 새 출력이 없다는 확인이 아닙니다. `retry_after_ms` 뒤 다시 읽으면 비동기 갱신 화면을 받을 수 있습니다.
- 읽을 때 기존 15초 원격 화면 표시 임대를 갱신하므로 숨겨진 warm 워크스페이스도 화면을 갱신합니다. 탭 전환을 강요하지 않습니다.
- 세션 종료·새 런타임 복원으로 generation이 바뀌면 입력을 거부하고 공유 권한을 다시 받아야 합니다.

## 로컬 저장과 재연결

앱 DB 폴더의 `cloud_agent_history.db`에 작업 수신증과 답변을 저장합니다. 최근 500건을 화면에 표시합니다. 원문 입력이나 토큰은 저장하지 않고 입력 바이트 수·Enter 여부·결과만 기록합니다. 중복 실행 방지를 위해 최대 100,000개의 요청 ID 수신증을 보존하고, 상한에 도달하면 새 작업을 거부합니다. 오래된 답변 본문은 500건 범위 밖에서 비웁니다.

Mac이나 Deppy가 꺼져 있으면 연결은 실패합니다. 봇은 나중에 다시 연결할 수 있으나 새 토큰 등록과 공유 선택이 필요합니다. Mac 종료 중 보낸 답변을 중계 서버에 보관하지 않습니다.

## 검증

```sh
cargo test -p agent-mcp
cargo test -p deppy-sijo --bin deppy-sijo cloud_agent
cargo test -p deppy-sijo --bin deppy-sijo ui::notifications
cargo test -p i18n
cargo build -p deppy-sijo --release
```

테스트는 로컬 HTTP와 App 권한/기록 및 egui 조작을 확인합니다. 실제 Grok Bot 계정 등록·인터넷 터널·봇의 도구 호출 준수 여부는 사용자 환경에서 별도로 연결 확인해야 합니다.
