# 자동 MCP 연결 구현 결과

날짜: 2026-09-27
버전: **0.1.0 → 0.2.0**
브랜치: `feat/automatic-mcp-connect`
작업 트리: `/Users/jr/Desktop/projects/deppy-sijo-performance`
제품 커밋: `bb0441a925d66d8b74602991a92c50018f1242d2`
동봉 도우미 커밋: `4c9786a9`

## 요청 해석 교정

이전 응답은 대안 검토·보고에 치우쳐 버튼에서 자동으로 주소를 생성하라는 구현 의도를 놓쳤다. 사용자 교정 후 자동 임시 MCP 연결을 구현하고 실제 공개 통신과 빌드를 검증했다.

## 사용 흐름

1. 설정의 클라우드 에이전트 MCP 화면에서 **자동 주소 생성**을 선택한다. 신규 설정의 기본값이다.
2. **MCP 연결 켜기**를 누르면 **주소 생성 중 → 외부 연결 확인 중 → MCP 연결됨**을 표시한다.
3. 실제 공개 MCP 응답이 확인되면 `https://…trycloudflare.com/mcp`와 주소·토큰 복사 버튼이 표시된다. 확인 전에는 복사할 수 없다.
4. Grokbot 등 커넥터에 주소를 등록하고 OAuth 승인을 진행하거나 토큰으로 인증한다. 대상 세션의 공유와 입력 허용은 별도로 켠다.
5. 봇은 기존 세션에 `send_text`로 입력하고 `read_output`으로 결과를 읽으며 `notify`로 자신의 답변을 해당 세션의 기록/알림으로 전달한다.
6. 연결 끄기와 정상 앱 종료는 접근 및 입력 권한을 취소하고 앱이 생성한 도우미를 종료·회수한다.

사용자는 cloudflared, Tailscale, Homebrew를 설치하거나 Cloudflare 계정을 만들 필요가 없다. 별도의 터널 터미널 창도 필요하지 않다. 앱에 포함된 검증된 도우미가 연결을 담당한다.

## 구현과 기존 기능 보존

- 공식 cloudflared2026.9.1 Darwin arm64/amd64 파일을 빌드 때 SHA256 검증 후 동봉한다. 실행 시 전역 PATH나 Homebrew를 사용하지 않는다. 패키지에 도우미와 Apache 라이선스를 넣고 서명한다.
- 별도 터미널 세션이나 LLM 에이전트를 생성하지 않는다. 앱이 소유한 네트워크 도우미 하나와 제한된 작업 스레드가 동작한다.
- 비밀 로그를 남기지 않고 stderr를 최대16KiB 행으로 계속 소비한다. private empty config, no-autoupdate, 상속된 TUNNEL_ 옵션 제거로 개인 고정 터널 설정의 개입을 방지한다.
- 엄격히 검증된 생성 호스트를 실제 서버의 Host/Origin 검사와 OAuth resource URL에 함께 반영한다. 공개 HTTPS metadata에서 정확한 resource가 확인돼야 준비 완료이다.
- 준비 완료 후10초, 확인 중1초 간격으로 실제 공개 응답을 점검한다. 연결 로그 하나의 해제만으로 전체 연결을 종료하지 않는다. 시작/복구90초 제한이 있으며 오류·취소에서는 접근을 취소한다.
- 준비/종료 중에만 UI 상태 갱신을 예약하고 연결된 유휴 상태에10fps 강제 화면 갱신을 추가하지 않는다.
- UUID/generation, 세션별 공유·입력 허용, 입력 회수, operation_id 중복 방지, 실제 PTY 수신증 및 자체 답변 기록을 유지했다.
- **고정 주소 직접 연결**을 유지한다. 기존 저장 호스트/포트는 이미 App에서 복원·저장되고 있었다. 새로운 모드만 기존 저장 경로에 추가했다. 구버전의 비어 있지 않은 고정 호스트는 수동 모드로 이관한다. 임시 주소는 고정 호스트 설정을 덮어쓰지 않는다.
- AGENTS.md에 제품 기능·버그·성능 배포마다 필수 버전 증가, lockfile 및 산출물 검증 규칙을 명시했다.

## 실제 검증

| 명령 | 결과 |
|---|---|
| `python3 -m unittest discover -s scripts/tests -p test_prepare_cloudflared.py` | 4 통과 |
| `cargo test -p agent-mcp` | 25 통과 |
| `cargo test -p deppy-sijo --bin deppy-sijo cloud_agent` | 21 통과, 공개 네트워크 테스트1 기본 제외 |
| `cargo test -p deppy-sijo --bin deppy-sijo config::tests` | 45 통과 |
| `cargo test -p i18n` | 8 통과 |
| `cargo run -p xtask -- i18n-check` | 통과 |
| `cargo run -p xtask -- check-boundary` | 통과 |
| `cargo run -p xtask -- check-deps` | 통과 |
| `cargo fmt --all -- --check`, `git diff --check` | 통과 |
| `sh -n scripts/dev-run.sh scripts/package-macos.sh scripts/verify-macos-package.sh` | 통과 |
| `cargo build -p deppy-sijo -p mcp-proxy --release` | 통과, 26.56초 |
| 아래 실제 공개 테스트 | 1 통과, 80.79초 |
| 개발 패키지 생성 및 서명·버전·아카이브 검증 | 통과 |

공개 테스트를 실제로 실행한 명령:

```sh
cargo test -p deppy-sijo --bin deppy-sijo automatic_public_tunnel_to_real_pty_and_own_answer_roundtrip -- --ignored
```

동봉 도우미가 생성한 HTTPS 주소로 실제 MCP 도구를 호출했다. 격리된 테스트 PTY에 입력을 보내고 출력을 읽고 봇 자체 답변을 기록했다. 동일 operation_id 재시도에도 실제 입력은1회였고 종료 후 도우미가 회수됐다. 이전 공개 시험도80.95초에 통과했으며 최종 건강 점검 로직에서 다시 실행한 결과가80.79초이다. OAuth resource는 공개 주소와 일치했다. 이 테스트에서 OAuth 승인 흐름은 로컬 HTTP로, 실제 도구 호출은 공개 HTTPS로 실행했다.

네이티브 Deppy 창이나 기존 사용자 세션은 테스트하지 않았다. 실제 Grokbot 계정의 커넥터 등록·로그인을 인증했다는 의미는 아니다. 최종 전체 workspace 테스트를 반복하지 않았으며 위 변경 영역 및 관련 경계만 검증했다.

## 코드 리뷰와 수정

1. 도우미 패키지 리뷰: ZIP에서 도우미/라이선스 동일성 비교 누락을 수정하고 재리뷰에서 추가 지적 없음.
2. 제품 소스 리뷰: 개별 unregister 로그가 전체 연결 상태를 잘못 종료할 수 있는 문제를 공개 응답 기반 점검으로 수정했다. 기존 고정 호스트 설정이 자동 모드로 바뀌는 문제는 설정 이관으로 수정했다. 각 문제의 실패 테스트를 먼저 실행한 뒤 최종 suite를 통과시켰다.
3. 최종 scoped Codex CLI 재리뷰: 추가 조치 가능한 지적 없음. 리뷰 대상은 변경 소스이며 문서는 제외했다.
4. 최초 병렬 suite에서 테스트 자식의 즉시 종료와120ms 타임아웃이 경쟁했다. 종료 테스트만2초로 바꾸고 실제 타임아웃 테스트는120ms를 유지했다. 재실행21개 통과. 원본 실패 로그를 보관했다.

## 빌드 산출물과 버전

- `target/release/deppy-sijo`, `deppy-mcp-proxy`, `deppy-cloudflared`
- `target/bundle/Deppy Sijo.app`, `Deppy Sijo.zip`
- 앱은0.2.0으로 컴파일했으며 About 메뉴는 `env!(CARGO_PKG_VERSION)`을 사용한다. 실제 bundle의 CFBundleVersion과 CFBundleShortVersionString 모두0.2.0임을 검사했다. About 창을 직접 열어 확인하지 않았다.
- 앱/프록시/도우미의 Developer ID 서명, arm64, strict 검증, ZIP 파일 동일성 검증을 실행했다. package script의 명시적 로컬 개발 모드로 만들었으며 **Apple 공증/Gatekeeper 배포 승인은 수행하지 않았다**.
- 실행 중인 이전 앱 PID67248을 종료하거나 재실행하지 않았다. 새 코드를 사용하려면 사용자가 재실행을 요청해야 한다. 푸시하지 않았다.

## 운영 범위

자동 모드는 Cloudflare 서비스 연결이 가능해야 하며 임시 주소는 다시 연결하면 달라질 수 있다. 그 경우 커넥터 주소도 다시 등록해야 한다. Cloudflare 차단 환경은 고정 HTTPS 주소 직접 연결로 다른 경로를 사용할 수 있다. 공급자 독립 고정 주소를 제공하는 운영형 Deppy 게이트웨이는 이번 구현에 포함하지 않았다.

앱 시작은 연결/입력 권한을 자동 복원하지 않는다. 자동 주소 생성은 버튼을 눌러 실행하며 인증 수명/회수는 기존 정책을 따른다.

## 재현 자료

`docs/reviews/measurements/2026-09-27-automatic-mcp-connect/results.json`에 명령과 실제 결과가 있고 같은 폴더에 최종 로그, 실패 재현, 리뷰, 산출물 SHA256·버전 검증을 보관했다. log-manifest.json은 EOF 중복 빈 줄 정리 전후 해시를 기록한다.
