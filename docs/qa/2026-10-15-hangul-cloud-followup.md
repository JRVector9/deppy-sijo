# 한글 입력·클라우드 완료 전달 후속 검토

2026-10-15. GPT-6.1-Sol / xhigh workers를 별도 worktree로 분리하고 root가 Cargo 단일 실행·교차 리뷰·통합을 맡았다. 이전 main PR231 제품0.12.2와 완료 증거 PR232는 통합 완료했다. 이번 문서는 그 이후 수정과 설계를 구분한다.

## 한글 입력 — source 수정과 실제 회귀 결과

기존 macOS winit backport를 반복 적용하지 않았다. [egui PR7983](https://github.com/emilk/egui/pull/7983)와 [PR8078](https://github.com/emilk/egui/pull/8078)의 focus→IME interruption 계약을 현재 locked0.36.1 소스와 대조했다. Workspace의 pending terminal focus가 첫 Preedit와 같은 frame에 적용되어 renderer의 기존 조합 보호를 우회했다. `request_focus`는 같은 widget에서도 IME interruption을 요청하며 egui-winit의 disable/enable 처리가 winit marked text를 비운다.

실제 test-only RED는0PASS1FAIL이었다. 첫 수정 뒤 새 empty-Preedit/지연-Commit 반례는2PASS2FAIL로 실패했다. 최종 수정은 기존 owner/preedit/pendingImeSubmit로 frame-local awaiting-commit을 계산하여 renderer와 Workspace automatic focus gate에 공유한다. 새 타이머·독립 대기 state·문자 치환은 없다. 명시 클릭과 TextEdit/popup exclusion은 유지한다.

통합 source: `crates/app/src/ui/workspace.rs`, `crates/terminal/src/renderer_egui.rs`, `crates/app/src/ui/workspace/tests/ime_input_harness.rs`. root commits4803bef6/7b662839/cddc2b0d. 실제 root GREEN: Workspace374PASS0FAIL2ignored, terminal renderer43PASS0FAIL1ignored. 이 회귀는 real egui UI frame과 PTY byte intent를 검증한다. 실제 사용자 macOS 입력기·언어 전환·PWA 키보드 성공을 실행한 증거는 아니다. 사용자 발생 위치 질문은 아직 미응답이다.

[winit PR4693](https://github.com/rust-windowing/winit/pull/4693)와 [cmux PR11806](https://github.com/manaflow-ai/cmux/pull/11806)는 입력 언어 전환 직후 첫 자모의 별도 cold-start 문제를 겨냥한 OPEN 제안이다. root도 원문 상태를 확인했다. 실제 해당 OS signature 없이 retry/timer를 backport하지 않았다. 상세 비교는 [GitHub 연구](evidence/2026-10-15-ime-cloud-followup/hangul-github-research.md).

## 최종 확인에서 발견한 두 flaky fixture

이전 main CI는 전체5733PASS였으나 두 테스트가 첫 실패 뒤 재시도로 통과했다. 첫 시도부터 통과했다고 기록하지 않는다.

문서 저장 회귀는 임시 파일이 생긴 것을 별도 스레드가 관찰한 뒤 외부 편집을 마치기 전에 저장이 먼저 commit할 수 있었다.8MiB 쓰기로 스케줄링을 늦추는 방식 대신 cfg(test) thread-local one-shot hook으로 실제 save_document의 temp sync 이후·최종 revision 검증 이전에 편집을 완료한다. Conflict/외부 bytes 보존은 유지하고 temp cleanup 검증을 추가했다. rootfdc27f6b. 실제16PASS0FAIL3ignored와 정확 회귀25/25 first-attempt PASS. 문서 제품 동작 변경은 없다.

MCP HTTP는 fixture만의 문제가 아니었다. private loopback production Server probe400요청에서 작은 요청은 정상,60KB 조기 거절은 EOF80/80 RST·그중78/80 head 유실, framed reader도71/80 실패였다. 같은 크기의 정상 initialize control은40/40완전응답이었다. 인증·Origin·중복 Authorization·protocol 검사에서 body를 읽기 전에 return하고 소켓을 닫으면 unread kernel body로 RST가 발생해 응답이 유실된다. reader만 Content-Length로 바꾸는 수정은 불충분하다.

root6796930c의 실제 새 HTTP RED는0PASS2FAIL이다. 조기401 framed 응답은 ConnectionReset, busy503은 완전 body 뒤 EOF read에서 ConnectionReset이었다. rootf5e58d5f에 응답 write half-close와 절대 시간·bytes cap cleanup을 통합했다. busy의 write+cleanup은 공동10ms, worker post-response cleanup은100ms, 미해석 요청은최대72KiB/해석된 body는64KiB 이하다. 정상적으로 body를 소비한 요청과 bodyless discovery는 추가 read를 하지 않는다. 인증·상태·dispatch 계약은 유지했다.

fresh root canonical0.12.3 production rlib로 독립 probe를 재링크한 결과400/400완전응답, RST/오류/누락/잘림0건이었다. 이전 large EOF80/80RST와 framed71/80오류는 모두0으로 줄었다. 실제 compile/run/분류 검증은 각각exit0이고 source/rlib SHA·mtime·root 단일 Cargo producer를 metadata에 기록했다. 이는 안전 cap 이내의 유효 framing/거절 경로에 대한 검사이며 과소 선언된 Content-Length 뒤 임의 surplus bytes나 cap을 넘은 malformed wire까지 응답 전달을 보장하지 않는다. 최종 workspace/Clippy 결과는 아래에 별도로 기록한다.

## 최종 로컬 검증과 릴리스 준비

source4e89cb1b(0.12.3)의 `cargo test --offline --locked --workspace --no-fail-fast -- --test-threads=1`은 실제 exit0, 78개 Cargo harness의 마지막 결과 합계 **5743PASS/0FAIL/65ignored**다. 중첩 subprocess summary3개는 중복 제외했다. App3233PASS/39ignored가 포함된다. [실행 메타데이터](evidence/2026-10-15-ime-cloud-followup/workspace-result.json)와 [압축 원로그](evidence/2026-10-15-ime-cloud-followup/workspace.log.gz)를 보존한다.

그 뒤 a4631474에 실제72KiB cleanup cap 회귀1개만 추가했다. 해당 source의 `cargo test --offline --locked -p agent-mcp -- --test-threads=1`은 실제 **74PASS/0FAIL/1ignored**다. 이 수치를 앞의 전체 검사 합계에 더하지 않는다. 최종 `cargo clippy --offline --locked --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `cargo run --offline --locked -p xtask -- check-boundary`, `cargo run --offline --locked -p xtask -- check-deps`도 각각 실제 exit0이다. [최종 gate 명령·종료 코드](evidence/2026-10-15-ime-cloud-followup/final-gates.json).

root가 공용 target의 유일한 Cargo producer였고 jobs2를 유지했다. peer review 후 실제 검증 결과를 기준으로 main PR과 0.12.3 로컬 빌드를 준비한다. 아직 0.12.3 artifact는 없으며 앱은 종료·실행하지 않았다. 신규 클라우드 알림은 설계 검토 대기이므로 이 bugfix release에 구현됐다고 표시하지 않는다.

## 클라우드 완료 알림 — 설계 검토 대기

현재 `notify`는 cloud→Deppy 방향이고 실제 Deppy→cloud 전송 channel이 없다. [설계 문서](../superpowers/specs/2026-10-15-cloud-local-completion-design.md)는 연결·세션별 opt-in 후 원 제출 RPC로 SSE 완료 결과를 자동 반환하는 방식을 권장한다. 별도 wait 도구를 agent가 알아서 호출해야 하는 간극을 없앤다. 표준 [MCP transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)와 [Tasks](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks)를 비교했다.

원 actor/operation/run/session/generation/native turn의 결속, UI acknowledgment와 독립된 완료 증거, 자식/승인 대기, 입력 개입·교체·재시작 unknown, actor/stream scoped replay, 권한 취소·상한·redaction을 설계했다. Socket 전송은 모델의 수신/인지 증거가 아니며, 실제 Dot/Meta Muse/Grok Bot 호환은 미검증이다. 사용자 설계 승인 async 질문은 대기 중이고 신규 기능 구현 코드는 없다. [상세 조사](evidence/2026-10-15-ime-cloud-followup/cloud-completion-research.md).

## 증거와 보존

[증거 manifest](evidence/2026-10-15-ime-cloud-followup/manifest.json)의 파일 bytes/SHA256은 실제 로그·test-only RED·production loopback probe·연구 문서를 보존한다. 성공·실패·ignored는 각 실행에 따로 기록하며 서로 다른 cohort의 PASS를 더해 전체 결과로 만들지 않는다. 원본 nominal/performance worktree dirty와 사용 중 앱·실계정·키·사용자 DB·clipboard·입력 언어는 조작하지 않았다. 앱 재실행도 하지 않았다.
