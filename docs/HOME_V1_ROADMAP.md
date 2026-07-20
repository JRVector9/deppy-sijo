# Home V1 roadmap

## 이번 브랜치 범위

- 실제 OpenAI·Anthropic 상태 공지, Hugging Face 트렌딩 모델, Grok 공식 상태 RSS를 제목 없는 업데이트 목록으로 표시한다. 목록은 최신 날짜순으로 정렬하고 한 번에 3행만 보여주며 나머지는 섹션 내부에서 스크롤한다. 공급자 데이터는 60분마다 갱신하고 수동 갱신도 같은 워커를 사용한다.
- Slack 공식 MCP endpoint를 Connector Center에서 바로 추가하고 기존 OAuth 흐름으로 연결한다.
- Slack 정책에 따라 Marketplace 게시 앱 또는 내부 앱의 고정 Client ID를 사용하고, PKCE와 `http://localhost:47456/callback` Redirect URL을 등록한다.
- OAuth 바인딩이 없으면 Connect 즉시 `https://api.slack.com/apps`를 열고, `Basic Information → App Credentials`에서 Client ID와 Client Secret을 복사하도록 안내한다.
- Slack의 인증 메타데이터를 확인한 뒤 일반 MCP용 authority 재확인과 실패가 확정된 DCR 시도를 생략하고 Client ID/Secret 입력창을 바로 표시한다. 저장된 client가 있으면 중간 확인 없이 Slack OAuth 승인으로 곧바로 진행한다.
- 첫 승인은 `team` 힌트 없이 Slack의 자체 워크스페이스 선택 화면을 사용한다. 성공한 token 응답의 `team.id`를 자동 저장해 다음 승인부터 같은 workspace로 고정하며, 사용자는 Team ID를 직접 입력하지 않는다. Slack이 계속 잘못된 workspace를 열 때만 `vector9.slack.com` 같은 주소를 선택적으로 받아 Slack 소유 HTTPS 페이지에서 Team ID를 확인하고, 저장된 workspace는 `다른 워크스페이스 선택`으로 다시 고를 수 있다. 현재 수동 앱 흐름에서는 고정 Redirect URL을 팝업에 보여주고 복사할 수 있게 하며, Slack 앱의 OAuth & Permissions에 최초 1회 등록하도록 안내한다.
- 사용자가 승인 대기 모달을 취소하면 callback listener를 즉시 닫는다. 고정 포트 `47456`은 짧은 종료 race만 재시도하며 이전 Deppy flow가 최대 대기시간 동안 포트를 붙잡지 않는다.
- Home 상단의 `Home` 제목과 설명 문구는 두지 않는다.
- 아직 데이터·실행 경로가 없는 카드는 숫자나 성공 상태를 꾸며서 표시하지 않는다.

## 후속 구현 우선순위

| 순위 | 기능 | 우선하는 이유 | 완료 기준 |
| --- | --- | --- | --- |
| P1 | Agents · Alerts · Activity | 앱 안에 실제 세션 상태, 입력 대기, 오류, 리소스 데이터가 이미 있어 가장 적은 비용으로 신뢰할 수 있는 홈 요약을 만들 수 있다. | 카드 수치와 활동 행이 기존 상태 소스와 일치하고 클릭 시 정확한 세션으로 이동한다. |
| P2 | Gmail · Calendar 연결 | 메일·일정이 홈의 실사용 가치를 가장 크게 늘리며 둘 다 하나의 Google OAuth 앱 정책으로 묶어 설계할 수 있다. | 계정별 OAuth, 최소 scope, 연결 해제, 메일/다음 일정의 실제 요약이 동작한다. |
| P3 | 수동 Workflow editor · Runs | 자동 실행 전에 DAG 저장, 단계별 입력·출력, 재시도와 감사 로그 모델을 먼저 안정화해야 한다. | 사용자가 만든 workflow를 수동 실행하고 단계별 결과와 실패 원인을 다시 열 수 있다. |
| P4 | Tasks와 예약 실행 | 앱 종료 중에도 실행하려면 별도 runner, 자격증명 접근, 재시작 복구가 필요해 수동 workflow 이후가 안전하다. | 시간대·중복 실행·실패 재시도 정책을 포함한 백그라운드 실행이 보장된다. |
| P5 | Skills 탐색·설치 | 공급망 신뢰, 버전 고정, 권한 설명과 업데이트 정책이 먼저 필요하다. | 서명/출처/요청 권한을 확인하고 설치·업데이트·롤백할 수 있다. |
| P6 | Services · Deployments · 추가 Connections | 공급자마다 API와 권한 모델이 달라 공통 connector contract가 검증된 뒤 넓히는 편이 유지보수에 유리하다. | 제공자 adapter가 동일한 상태·오류·연결 해제 규약을 따른다. |
| P7 | Community/“Built with” 카탈로그 | 배포·검수·신고·평가 시스템 없이는 정적 샘플에 머물 가능성이 높다. | 실제 게시·검수·설치 흐름과 신뢰 지표가 준비된다. |

## 구현 원칙

- Home은 각 기능의 원본 상태를 요약할 뿐 별도의 상태 진실 소스를 만들지 않는다.
- 숫자와 연결 상태는 실제 API/DB 결과만 표시한다.
- OAuth client secret과 access token은 소스·DB·로그가 아니라 OS keyring에 저장한다.
- 외부 공급자 추가는 `연결 → 권한 확인 → 연결 상태 → 관리/해제` 흐름을 공통으로 사용한다.
