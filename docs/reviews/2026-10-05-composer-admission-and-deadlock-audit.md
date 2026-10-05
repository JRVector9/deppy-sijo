| Priority | Location | Finding | Impact | Next step |
|---|---|---|---|---|
| critical | crates/connector-ui/src/popup/mod.rs:34 | Context 재진입 잠금 교착; 소스 수정 및 재검증 완료 | UI 전체 정지 | 새 버전 빌드에 포함 후 명시적으로 재실행 |
| high | crates/runtime/src/input_admission.rs:203 | 이전 스트림 상태가 직접 입력까지 거절; 소스 수정 완료 | 빈 입력창에서도 전송 불가 | 실제 사용자 세션에서 새 버전 전송 확인 |
| high | crates/agent-mcp/src/server.rs:93 | 인증 잠금 안에서 알림 콜백 호출; 소스 수정 완료 | 재진입 시 인증/종료 처리 정지 | 새 버전에 포함 |

## 작업 범위와 원인

사용자 요청 순서: 기존 UI 교착 재점검, 하단 입력 거절 수정과 Warp 비교, 이후 다른 교착 점검. 최신 제품 소스가 있는 `/Users/jr/Desktop/projects/deppy-sijo-performance`에서 작업했다. 원래 작업 디렉터리의 다른 브랜치와 기존 수정은 건드리지 않았다. 실행 중 앱에 지시를 전송하거나 앱을 시작/종료하지 않았다.

팝업 교착은 이전에 캡처한 PID40395/0.7.3의 메인 스택과 실제 egui Context 동시 읽기/백그라운드 repaint 테스트로 확인했다. `ctx.data` 잠금 안에서 다시 Context를 조회하던 코드의 viewport ID와 pass를 바깥에서 먼저 읽는 수정이 유지되어 있다. 이번에도 같은 동시성 회귀 테스트를 다시 실행해 통과했다. 자세한 스택과 최초 RED는 `2026-10-05-live-ui-freeze-deadlock.md` 참조.

하단 전송에서는 별도의 편집 버퍼가 이미 존재하지만, 직접 전송과 자동 예약 전송이 `AgentInputGuard`를 공유한다. `StatusDetector`의 스트림 Waiting/NeedsApproval은 입력을 받아야 해제되는 반면 guard가 그 상태에서 입력을 거절한다. 실제 소유 PTY에서 이전 상태를 출력하고 화면을 지운 뒤 원래 빈 Claude 입력/흐린 Codex 안내 입력으로 돌아와도 긴 한글 전송이 AdmissionDenied로 거절되는 순환 조건을 재현했다.

수정은 명시적 직접 제출에만 적용한다. 현재 커서 행에서 빈 입력이나 안내 문구를 확인하고 현재 화면에 질문/승인 요청이 없을 때 이전 StreamRegex 상태가 제출을 막지 않는다. 자동 전송은 기존 조건을 유지한다. 실제 선택창, 화면의 승인 질문, 이미 수용된 터미널 초안, 이력 recall, 다른 foreground, permit/credential/deadline 및 출력 갱신 확인을 유지한다. 동일 우선순위의 현재 승인도 StreamRegex source를 유지할 수 있어 source만으로 판단하는 첫 수정은 보호 테스트에서 실패했다. 현재 화면 요청의 유무를 별도로 확인하도록 바로 보완했으며 이 중간안은 출시되지 않았다.

거절 로그에는 `foreground_changed`, `detector_unavailable`, `accepted_input_draft`, `current_choice_dialog`, `current_input_request`, `visible_input_draft`, `automatic_prompt_not_verified_empty` 같은 고정 태그와 provider/intent만 기록한다. 사용자 본문, 화면 셀, 토큰은 기록하지 않는다. App에서 target/checkpoint 때문에 거절되는 경로는 이 runtime 태그의 범위 밖이다.

추가 점검에서는 `Auth::oauth_route`가 Credential Mutex를 잡은 상태에서 OAuth 승인 알림 콜백을 호출하는 별도 교착을 재현했다. 콜백이 `auth.approvals()`/`expires()`를 읽으면 같은 잠금을 기다린다. 실제 HTTP head와 OAuth 등록/인증 라우트를 사용하는 소유 subprocess가 수정 전 5초 동안 멈췄다. 인증 상태를 잠금 안에서 먼저 반영하고 알림 의도만 기록한 뒤 잠금을 해제하고 콜백을 호출하도록 수정했다. 정상 알림 1회, 콜백의 승인 조회, 등록/실패 요청에서 알림 없음까지 검증했다. 실제 앱 정지의 원인은 팝업 교착으로 캡처되었고, 이 OAuth 문제는 별도 재진입 조건에서 확인된 문제다.

이전 코드 리뷰에서 완료한 App 수정도 보존했다. 저장 완료가 늦게 도착해도 만료된 제출은 자동 발송하지 않고 초안을 유지한다. Composer 거절/Unknown은 입력 카드에서 처리하며 창 밖 native 알림을 만들지 않는다. 관련 RED/GREEN과 전체 테스트는 `2026-10-05-composer-checkpoint-stall-code-review.md` 참조.

## Warp 비교

비교 기준은 공개 저장소의 고정 소스 `b865631c9a0e46b548c7ec7dc32e228a148171d1`이다. Warp 코드를 복사하거나 빌드하지 않았다.

- [InputBufferModel](https://github.com/warpdotdev/warp/blob/b865631c9a0e46b548c7ec7dc32e228a148171d1/app/src/terminal/input/buffer_model.rs)은 편집기 내용과 커서 변경 이벤트를 구독한다.
- [CLI 입력 상태](https://github.com/warpdotdev/warp/blob/b865631c9a0e46b548c7ec7dc32e228a148171d1/app/src/terminal/cli_agent_sessions/mod.rs)는 터미널별 초안과 rich input 열림/닫힘을 관리한다.
- [Enter/제출 처리](https://github.com/warpdotdev/warp/blob/b865631c9a0e46b548c7ec7dc32e228a148171d1/app/src/terminal/input.rs#L13923)는 메뉴 선택과 Enter 설정을 먼저 처리한 뒤 편집기 본문을 SubmitCLIAgentInput 이벤트로 보낸다. 전송에는 대상 터미널이 필요하지만 편집 내용과 제출 의도는 별도 상태다.

Deppy의 편집 버퍼 전체를 교체할 필요는 없다. 확인된 문제는 편집기 자체보다 직접 전송에 남아 있던 자동화 상태 제약이다. 이번 수정은 직접 제출과 자동화의 판단을 구분하며 실제 대상/초안/승인 보호를 유지한다.

## 추가 교착 점검 범위

전체 `crates` Rust 파일 299개, Context/Ui accessor closure 301개를 문자열/주석 마스킹과 괄호 균형 기반으로 읽기 전용 조사했다. 수정 후 직접 Context를 재호출하는 후보는 없었다. 이는 간접 함수 호출이나 모든 스케줄링 경로의 부재를 증명하는 정적 분석은 아니다.

수동으로 popup target/geometry/footer, worker lifecycle/admission/idle exit/shutdown, draft saver, runtime emit/receiver slots, permit→credential→PTY input queue, PTY output cancellation, SQLite write worker, native key queue, Codex app-server event backlog를 확인했다. Runtime wake는 subscribers/viewport 잠금 해제 후 실행된다. 저장 I/O와 wake는 draft saver 공유 잠금 밖이다. 워커 종료는 result/job 채널을 닫아 발행/수신 대기를 해제하고, runtime은 shutdown flag+unpark를 사용한다. PTY는 cancel/receiver 종료 후 join한다. 새롭게 재현한 교착은 위 OAuth 콜백 경로다.

Codex app-server의 stdio write는 동기 `write_message` 경로를 유지한다. 이 경로의 pipe backpressure와 종료 응답성은 별도의 실제 재현이 필요하며, 이번 조사에서 교착으로 확인하지 않았다. 이전 Server is draining 오류도 현재 공유 데몬에서는 독립적으로 재현되지 않았다. 공유 데몬은 종료/재시작하지 않았다.

## 실제 검증 기록

- `/tmp/deppy-composer-stale-red-20261005.log`: 0 pass / 1 fail. 현재 입력이 돌아왔는데 이전 stream 상태 때문에 긴 제출 거절.
- `/tmp/deppy-composer-current-approval-red-20261005.log`: 0 pass / 1 fail. 중간 수정이 같은 우선순위의 현재 승인 상태를 덮어쓸 수 있음을 확인하고 보완.
- `/tmp/deppy-composer-stale-final-green-20261005.log`: 새 실제 PTY 2 + 기존 pr2 3 + pr13 3 = 8 pass.
- `/tmp/deppy-composer-and-popup-controls-20261005.log`: 긴 입력/placeholder, 현재 요청/초안, 팝업 교착 재검증 4 parent tests pass. subprocess 결과를 중복 합산하지 않는다.
- `/tmp/deppy-composer-deadlock-full-gates-20261005.log`: Connector 24, App 2787 pass / App 34 ignored. 병렬 Runtime 실행은 344 pass / 3 fail: known_hosts 임시 파일 생성, 전역 capture-reader 수 측정, TLS 이벤트 시간 제한.
- `/tmp/deppy-composer-deadlock-runtime-serial-gates-20261005.log`: 전체 Runtime 347 + Session 75 pass, strict App/Runtime/Session/Connector all-target Clippy `-D warnings`, boundary, 27-crate dependency gate, fmt pass. 최초 병렬 실패를 삭제하거나 병렬 전체 통과로 기록하지 않는다.
- `/tmp/deppy-oauth-wake-deadlock-red-20261005.log`: 0 pass / 1 fail, 소유 subprocess가 5초 멈춤; 그 자식만 종료/회수.
- `/tmp/deppy-oauth-wake-deadlock-green-gates-20261005.log`: Agent MCP 31 pass / 1 ignored, App cloud-agent 54 pass / 3 ignored, strict Agent MCP/App all-target Clippy, boundary/deps/fmt pass.
- `/tmp/deppy-composer-deadlock-app-final-20261005.log`: OAuth 수정 후 최종 App 전체 2787 pass / 34 ignored, fmt pass; gate exit0. Runtime347/Session75/Connector24/AgentMCP31을 합쳐 중복 없이 3264 pass / 35 ignored. 실제 사용자 앱에서 전송 확인은 포함되지 않는다.
- `git diff --check`: pass.

## 출시와 남은 확인

제품 버전은 0.7.3이며 이번 작업은 소스/테스트 수정이다. 아직 새로운 실행용 앱을 제공하지 않았다. 변경 제품을 빌드해서 제공할 때는 마지막 제공 버전보다 높은 버전, lock workspace 27개, native/About 및 macOS plist 두 버전, 서명/소스 일치 검증이 필요하다. 앱 재실행은 현재 작업에서 별도 명시 요청이 없어서 수행하지 않았다.

소유 fixture에서 확인한 상태 순환과 긴 입력 전송을 수정했으나, 실제 사용자 세션에서 나온 모든 거절의 원인이 같은 것이라고 단정하지 않는다. 새 버전이 실제로 실행된 뒤 실패한 세션의 App target/checkpoint/runtime 거절 위치와 현재 화면 상태를 확인해야 한다. 이미 기록된 전송에 대한 자동 재시도는 하지 않는다.
