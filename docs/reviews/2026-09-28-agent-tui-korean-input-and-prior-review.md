# AI 터미널 입력창의 빠른 한글 입력 및 이전 커밋 리뷰

2026-09-28 · 브랜치 `fix/agent-tui-korean-submit-order` · 소스 커밋 `2d7b6f84` · 버전 0.2.2 → 0.2.3

## 조사와 근거

- Deppy의 AI TUI 입력은 `egui` raw 이벤트를 순회해 `WriteInput`으로 PTY에 보낸다. `Key::Enter`가 `Ime::Commit`보다 먼저 오면 기존 경로가 `\r요`를 전송했다. 실제 `egui_kittest` 프레임 테스트 RED에서 `[13,236,154,148]`을 확인했다. 사용자 기대는 `요\r`이다.
- [Warp #8919](https://github.com/warpdotdev/warp/issues/8919)는 Claude/Gemini CLI에서 마지막 조합 글자가 Enter에 앞서 확정되지 않아 빠지는 증상을 보고한다. Warp 구현이 공개된 것은 아니므로 이 이슈는 증상 비교 자료다.
- [cmux #3762](https://github.com/manaflow-ai/cmux/issues/3762)와 [수정 PR #3867](https://github.com/manaflow-ai/cmux/pull/3867)은 활성 IME 종류만 보고 모든 방향키·Space를 가로채면 한국어 사용자에게 회귀가 생긴다는 근거다. Deppy는 활성 조합과 현재 터미널 소유 세션에만 제출 보류를 적용한다.
- [cmux #7708](https://github.com/manaflow-ai/cmux/issues/7708)은 빠른 입력의 순서가 PTY 입력 경계에서도 깨질 수 있음을 별도로 보고한다. Deppy의 이번 재현은 원격 전송보다 앞단의 UI 이벤트 → PTY 바이트 순서였다.

## 변경

- 현재 조합 중인 터미널 세션에서 Enter/Shift+Enter가 Commit보다 먼저 오면 Enter와 이후 키를 해당 세션에 보류한다. 같은 프레임 또는 다음 프레임의 Commit을 먼저 보낸 후 순서대로 방출한다.
- AppKit local key monitor가 Return 전후의 실제 ASCII 문장부호 위치를 표시한다. IME가 Text를 생략한 문장부호는 물리적 Return 경계를 넘지 않도록 복구한다. Commit 뒤의 Text가 Return 앞 문장부호를 끌고 가는 경우도 방지한다.
- 보류 중인 다음 프레임도 IME 복구 경로로 처리한다. Commit이 2초 내 오지 않으면 보류한 입력을 PTY에 전달하고, Commit이 같은 프레임에 있으면 Commit을 우선한다. 보류 크기가 2 MiB를 넘으면 입력을 버리지 않고 방출한다. 포커스/입력 소유권 변경 시에는 다른 세션으로 입력을 옮기지 않는다.
- 클립보드 읽기 결과도 보류 중인 같은 세션의 입력 순서에 포함한다. pane 전환에서는 보류 입력을 원래 세션으로 보낸다. 전후 문장부호 복구는 같은 프레임/다음 프레임의 다음 키보다 앞에 삽입한다.

## 실제 검증

- RED: Enter 후 Commit `\r요` vs 기대 `요\r`; 다른 프레임의 Enter 선전송; Enter-Tab-Commit `\t한\r` vs `한\r\t`; Commit 다음 Text에 전단 문장부호 `.,` 동반; 오래된 보류의 Enter 누락; Commit 부재 시 보류 입력 누락. 각 경계의 RED를 확인한 후 수정했다.
- GREEN: `cargo test -p deppy-sijo --bin deppy-sijo 한글 -- --test-threads=4` 19 통과, `ime` 68 통과, `preedit` 8 통과, 추가 문장부호/기한/네이티브 원장 테스트 통과. `cargo fmt --all --check` 및 `git diff --check` 통과.
- `cargo test -p deppy-sijo --bin deppy-sijo ui::workspace::tests:: -- --test-threads=4` 최종 실행은 **288 통과, 기존 실패 2개**(`agent_info_line`의 `Idle`/`Awaiting instruction`, `attached_without_snapshot`의 폭 80/116`)였다. 이 변경의 입력 경로와 무관한 기존 기대치 문제다. 전체 workspace 통과로 기록하지 않는다.
- `cargo check -p deppy-sijo -p mcp-proxy`와 **최종 소스** `cargo build --release -p deppy-sijo -p mcp-proxy` 통과. 별도 `target/bundle-0.2.3`에 최종 앱 바이너리를 넣고 Developer ID로 재서명했다. `codesign --verify --deep --strict`, 3개 바이너리 arm64 확인, `CFBundleVersion`과 `CFBundleShortVersionString` 모두 0.2.3 확인, ZIP 재생성을 수행했다. 네이티브 macOS 한글 물리 입력은 앱 재실행 권한이 없어 검증하지 않았다.

## 이전 커밋 코드 리뷰

읽기 전용 Codex CLI로 `bffcf483`, `4ad811af`, `76b326df`, `f67e1290`, `6cabc21b`의 실제 소스 변경과 연결 호출부를 검토했다. 한 건의 개선점을 찾았다.

| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| medium | `crates/app/src/cloud_agent/tunnel/quick_dns.rs:79-129` | 시스템 DNS 실패 뒤 DoH의 A 응답이 있으면 AAAA를 조회하기 전에 반환한다 | IPv6만 통하는 환경에서는 IPv4 주소만으로 외부 연결 확인에 실패할 수 있다 | 별도 DNS 성능/가용성 변경에서 실제 IPv6 경로와 시간 예산을 재현한 뒤 주소 선택을 개선 |

이 지적은 코드 경로상 가능한 조건이다. 현재 Mac에서 IPv6 전용망 재현이나 실제 GrokBot 계정 검증을 수행한 결과는 아니다. 기존 빠른 주소 게시 속도를 늦추지 않으려면 별도 계측이 필요하다. 그 외 네 커밋에는 리뷰 범위 내에서 재현 가능한 신규 결함을 찾지 못했다. CI는 이전 PR #199–#201에서 GitHub 청구 제한으로 작업이 시작되지 않은 상태다.

이번 수정은 [PR #202](https://github.com/JRVector9/deppy-sijo/pull/202)로 PR #201 위에 게시했다. #202의 첫 체크 목록에는 실패와 대기가 섞여 있다. 상세 작업 조회는 GitHub API rate limit(HTTP 403)으로 막혀 원인을 확인하지 못했다. #202 실패 원인을 앞선 PR의 청구 제한으로 단정하지 않는다.

## 남은 검증 범위

- pane 전환 직전 아직 Commit이 없으면 화면에 보이던 preedit을 원래 세션에 먼저 보낸다. AppKit이 그 뒤 늦게 Commit을 다른 워크스페이스에 전달하는 경우까지 실제 Mac 키보드로 검증하지 못했다. 새 pane에 중복 글자가 생기는지 확인하려면 앱 재실행이 필요하다.
- Text가 사라진 문장부호는 AppKit의 Return 전후 정보로 복구하지만, 같은 구간의 여러 Tab/방향키 사이 정확한 상대 순서는 현재 관찰기만으로 알 수 없다. 클립보드 읽기가 느리고 그 사이 키가 입력되면 결과는 읽기 완료 시점에 큐에 들어간다. 각각 최종 소스 리뷰의 medium 경계다.
- 네이티브 GUI를 실행하거나 기존 `target/bundle`의 앱을 재실행하지 않았다. 검증한 것은 실제 Deppy 입력 처리 함수와 headless egui 프레임/PTY 명령, 릴리스 빌드 및 서명이다.
