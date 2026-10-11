# 세션 시작 직후 하단 컴포저 입력 거절 조사 — 2026-10-11

## 요청과 조사 범위

사용자: 세션 시작 후 바로 하단 입력창에 작성·전송하면 입력되지 않는다. **수정하지 말고 원인을 확인하여 PR 단위 개선안만 정리한다.** 후속 기준: 이미 보냈던 문장이라도 사용자가 새로 작성하고 전송하면 새 입력이다.

제품 코드, 버전, 앱 설정은 수정하지 않았다. 실행 중인 앱을 종료·재시작하지 않았고, 사용자 에이전트에 시험 프롬프트를 보내지 않았다. 임시 진단은 별도 로컬 `/bin/cat` PTY와 제품 모듈을 사용했다. 이 문서와 PR 계획, handoff만 이번 조사 결과물이다. 실제 GitHub PR/commit/push는 만들지 않았다.

## 실행본과 소스

- 실행 PID: 21173. executable: `target/restart-0.10.1-20261011/Deppy Sijo.app/Contents/MacOS/deppy-sijo`.
- 실행 버전: 0.10.1. 소스 기준: `799fae89de82ec9a0e57104b47d18cf20d274186` + 기존 미커밋 작업.
- authoritative 작업 트리: `/Users/jr/Desktop/projects/deppy-sijo-performance`. nominal checkout `/Users/jr/Desktop/projects/deppy-sijo`는 수정하지 않았다.
- 기존 다른 작업의 history/resize 변경이 존재한다. 현재 소스의 후속 resize 수정은 실행본에 적용됐다고 주장하지 않는다. 이번에 직접 추적한 `status.rs`, `input_admission.rs`, `composer.rs`, `agent_detect.rs`는 조사 중 변경하지 않았다.

## 결론과 확실성

| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | runtime/src/input_admission.rs:258 | 과거 accepted draft가 ExplicitPrompt를 차단 | 실제 Codex 전송 5회 PTY 쓰기 전 거절 | PR1로 발생 입력 연계, PR2로 판정 수정 |
| high | session/src/status.rs:456 | 제어 키도 초안으로 기록되고 재그리기로 해제되지 않음 | 글자가 없는 Codex 편집기도 계속 전송 거절 | PR2 입력 intent·편집기 증거 반영 |
| high | app/src/app.rs:31324 | target만 있으면 전송 가능, UnconfirmedAi는 후단에서 거절 | 시작 직후 준비 이전 전송 실패 경로 존재 | PR3 준비 대기와 정확한 대상 고정 |
| medium | runtime/src/input_admission.rs:233 | 거절 로그에 세션·요청·flag 생성 근거 없음 | 이번 사건의 최초 입력을 특정할 수 없음 | PR1 내용 없는 연계 진단 |

**이번 5회 거절의 직접 원인은 확정했다.** `accepted_input_draft` 판정이 PTY 쓰기 전에 거절한다. 제품 코드로 동일 차단을 만드는 잘못된 상태 전이도 재현했다.

**그러나 이번 사건에서 어떤 최초 키/조작이 dirty 상태를 만들었는지까지 100% 확정한 것은 아니다.** 현재 INFO 로그에 해당 이력이 없다. Codex 강도 키가 이번 사용자의 실제 직전 조작이었다고 단정하지 않는다. 세션 시작의 실행 감지 공백도 별도의 코드 경로이며, 실제 5회 로그의 사유와 혼동하지 않는다.

## E1. 실제 실행 로그: 요청이 runtime까지 도달한 뒤 거절됨

실제 로그 파일: `/Users/jr/Library/Application Support/app.vector9.deppy-sijo/logs/app.log.2026-10-11`, L46–50.

| KST | intent | provider | reason |
| --- | --- | --- | --- |
| 10:57:32.400428 | ExplicitPrompt | Codex | accepted_input_draft |
| 10:57:33.884603 | ExplicitPrompt | Codex | accepted_input_draft |
| 10:57:34.150494 | ExplicitPrompt | Codex | accepted_input_draft |
| 10:57:34.299784 | ExplicitPrompt | Codex | accepted_input_draft |
| 10:57:34.449876 | ExplicitPrompt | Codex | accepted_input_draft |

`AgentInputGuard::allows` → false → `Worker::admit_input_batch_checked_with_log_sink` L2708 → `AdmissionDenied` → `InputAdmitted` → App `observe_prompt_admissions` → composer Rejected 순서다. `active.write_input_batch(parts)`보다 앞에서 거절하므로 이 5건은 PTY에 본문/Enter를 예약하지 않았다.

이는 문장 내용 중복을 검사한 결과가 아니다. 거절 분기는 새 문장의 내용·길이를 비교하지 않고 detector의 bool 상태만 본다. 같은 이유가 계속 남으면 다시 Enter를 눌러도 같은 분기로 간다.

## E2. 오래된 초안 상태가 새 입력을 막는 구조

`crates/session/src/status.rs`:

- L398: `has_input_draft()`는 `input_draft_dirty`를 그대로 반환한다.
- L456–540: 수락된 입력 바이트를 분석하여 일반 글자, 일부 history/CSI 키를 dirty로 기록한다.
- L483–487: bracketed paste 밖의 CR/LF/Ctrl+C에서 dirty가 해제된다.
- L625: output redraw는 dirty를 해제하지 않는다.
- L598: turn start는 상태를 Running으로 바꾸지만 dirty는 해제하지 않는다.

`crates/runtime/src/input_admission.rs` L258–265:

- ExplicitPrompt도 dirty가 있으면 거절한다.
- 예외는 명시적 전송 + 현재 행이 정확히 Empty + 마지막 erase 입력 뒤 output redraw + 현재 승인/질문 없음이다.
- **Placeholder는 이 예외에 포함되지 않는다.** Codex가 Ctrl+U 등으로 글자를 지운 후 기본 안내문을 그리면 실제 글자가 없어도 계속 거절할 수 있다.

이 기억은 현재 CLI 실행 identity에 묶인 별도 초안 모델이 아니다. runtime session detector에 남는다. 실행 감지는 별도 `AgentExecutionIdentity`로 갱신되므로 같은 PTY 안에서 CLI가 바뀌어도 draft가 자동으로 그 새 실행에 맞춰 재검증되는 경로는 없다. epoch 변경만으로 현재 실제 초안을 지우는 구현도 옳지 않으며, PR2에서 current editor 근거와 함께 다뤄야 한다.

## E3. 잘못된 dirty 상태를 만드는 구체적인 경로

Codex effort 키는 `crates/app/src/pty_effort.rs` L102–103의 Shift+Up / Shift+Down (`ESC[1;2A`, `ESC[1;2B`)이다.

사용자 강도 조절 경로:

1. App의 `EffortPlan::Keys` → `writes`.
2. `write_all_to_session` L25175 → 일반 `RuntimeCommand::WriteInput`.
3. runtime이 수락하면 모든 일반 입력과 같이 `detector.on_user_input`을 호출한다.
4. detector는 CSI 최종 A/B를 history recall 가능 입력으로 취급하여 dirty를 켠다.
5. Codex 화면은 강도만 바뀌고 입력줄은 비어 있어도, 다음 하단 새 문장 전송을 거절할 수 있다.

runtime L2738에는 **AutomaticPrompt + Codex guard를 가진 단일 effort key**에만 draft 기록을 생략하는 예외가 있다. 예약 작업 강도 설정용 경로는 보호되어 있지만, 일반 WriteInput을 사용하는 수동 강도 조절 경로에는 그 예외가 적용되지 않는다. 같은 바이트라도 생산 경로별로 다르게 판정하는 결함이다.

이 경로는 직접 재현됐다. 이번 사건의 실제 직전 키였는지는 E1 로그만으로 판단할 수 없다.

## E4. 세션 시작 시 별도로 존재하는 준비 상태 공백

`crates/app/src/app.rs` L31250–31325:

- composer는 세션 target이 있으면 `can_send=true`.
- 감지된 `agent.execution`이 없고 Agent session이면 `UnconfirmedAi`를 캡처한다.
- 그 context는 checkpoint를 거쳐 같은 제출 요청에 유지된다.
- L31770–31780의 `queue_prompt_delivery`는 `UnconfirmedAi`를 즉시 거절하며 준비 완료까지 보관하지 않는다.

종류 감지는 별도 worker이며 `agent_detect_worker.rs` L23의 kinds 기본 주기가 1,200ms다. 최초 감지 지연이 항상 1,200ms라는 뜻은 아니지만 세션 target 생성과 CLI 실행 증명 도착은 동일 순간이 아니다. 감지가 나중에 완료돼도 이미 캡처된 UnconfirmedAi 요청이 자동으로 준비 상태로 승격되지는 않는다.

또 L12704의 single controller slot이 세션 시작/다른 요청에 사용 중이면 composer 요청을 대기시키지 않고 거절한다. 기존 `pr1_busy_host_slot_does_not_erase_draft_or_history` 테스트는 초안을 보존하고 재시도를 허용함을 확인하지만, 최초 전송 완료를 보장하지 않는다. 실제 E1 요청은 runtime에 도달했으므로 E1의 직접 원인은 이 두 경로가 아니다.

## E5. 새로 쓴 동일 문장은 새 요청이라는 기준

- 현재도 정상적으로 settled된 요청 뒤의 같은 본문은 content hash로 차단하지 않는다.
- `try_submit` L1242는 매번 새 `submission_id`를 만들고 runtime delivery L31796은 새 UUID operation을 만든다.
- ACK 때의 `prompt == original` 비교는 원래 요청과 결과를 연결하기 위한 검사다. 본문 중복 제거가 아니다.
- 별도 개선점: `submission_blocked` L1269는 동일 draft key에 pending/Unknown 결과가 남으면 **새로 작성한 내용도** 전송을 막는다. 이 경로는 이번 E1 거절과 구분한다.
- 사용자 기준은 새 전송 동작마다 새 요청이다. 이전 요청의 실제 수신 여부가 모호한 경우에도 그 결과는 그 요청에만 남기고, 이후 독립된 작성·전송을 이전 본문과 합쳐 취급하지 않는 계약을 PR4에 정의한다. 결과가 불명인 이전 요청의 자동 재전송은 이 기준에 포함되지 않는다.

## 직접 실행한 검증

진단 디렉터리: `/private/tmp/deppy-composer-startup-investigation-jop_yiyy`.

1. `detector_probe.rs`: 제품 `status.rs`를 직접 `#[path]`로 로드한 별도 Rust 프로그램. compile exit0, run exit0. fresh=false; 같은 본문+CR 2회 모두 dirty=false; Shift+Up/Down 후 redraw/turn-start에도 dirty=true; early text 유지; erase 뒤 erase_redrawn=true; 완전한 paste+CR은 false.
2. `guard_probe.rs`: 제품 `input_admission.rs` 직접 로드 + 정확한 Cargo session dependency fingerprint의 라이브러리 + 별도 cat PTY. compile exit0, run exit0. fresh Placeholder/동일 본문 재제출 허용; effort 키 후 Empty/Placeholder 모두 거절; Ctrl+U+redraw 후 Empty 허용/Placeholder 거절; CR 후 다시 허용. 사용자 앱에는 입력하지 않았다.
3. 기존 runtime test executable의 관련 입력 보호 회귀 **7 passed / 0 failed**. 현재 기존 동작을 검증한 것이며 수정 후 회귀 검증이 아니다. 전체 suite를 다시 돌리거나 앱 E2E를 했다고 주장하지 않는다.

| 실행 필터 | 결과 |
| --- | --- |
| composer_ | 4 passed / 0 failed |
| followup_codex_effort_keys_do_not_block_next_step_or_prompt | 1 passed / 0 failed |
| pr2_actual_admission_preserves_existing_tui_draft_and_dialog | 1 passed / 0 failed |
| pr2_automatic_unknown_and_wrong_foreground_fail_closed_but_explicit_other_is_useful | 1 passed / 0 failed |

실행 바이너리: `target/debug/deps/runtime-de2eab1d786680d9`. source/rlib hashes와 compile/run exit, test 결과는 임시 `provenance.json`, `guard-provenance.json`, `runtime-existing-results.json`에 기록했다. 재현 출력은 `probe.log`, `guard-probe.log`, `runtime-existing-{0,1,2,3}.log`.

실패 접근: 첫 guard probe는 서로 다른 Cargo dependency cohort의 최신 rlib를 섞어 타입 불일치로 컴파일 실패했다. session fingerprint의 정확한 dependency rlib를 찾아 재컴파일하여 exit0을 확인했다. 제품 코드는 변경하지 않았다.

## 읽기 전용 실제 화면 확인과 한계

현재 앱의 nomorevibe 및 Design 두 세션을 읽기로 확인했다. Codex의 현재 빈 Placeholder 편집기를 관찰했다. 관찰 시각은 E1보다 뒤이므로 이를 E1 당시 상태로 소급해 원인을 단정하지 않았다. 사용자 입력/전송 없이 원래 nomorevibe 화면으로 돌아왔다. DB는 `mode=ro`로 세션 메타데이터만 조회했으며 prompt/credentials를 보고서에 복사하지 않았다.

발생 워크스페이스/세션·증상 종류를 async 질문으로 확인 요청했으나 조사 시점에 답변은 없었다. 이는 E1의 runtime 거절 사실을 바꾸지는 않지만, 특정 세션에 최초 dirty를 남긴 입력을 역추적하는 증거가 부족하다. **해당 연계 증거가 확보되기 전에는 이번 사건의 최초 유발 키를 100% 확정했다고 보고하지 않는다.**

다음 구현은 [PR 계획](../superpowers/plans/2026-10-11-composer-startup-input-pr-plan.md)에만 정리했다. 현재 수정/배포/재실행은 하지 않았다.
