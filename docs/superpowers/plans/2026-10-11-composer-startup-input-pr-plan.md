# 하단 컴포저 입력 개선 PR 계획 — 2026-10-11

상태: **조사와 계획만 작성. 구현 미착수.** 사용자 요청은 수정 금지다. GitHub PR/commit/push/release/restart는 수행하지 않는다.

근거: [원인 조사·실행 증거](../../qa/2026-10-11-composer-startup-input-investigation.md).

## 동작 기준

1. 사용자가 새로 작성·전송한 문장은 이전과 본문이 같아도 새 submission/operation이다. 본문·history·옛 draft 상태로 새 요청을 중복 처리하지 않는다.
2. 작성 중인 buffer는 CLI 감지, PTY 상태, 저장 worker, 과거 ACK와 독립적이다. 새 전송에서 캡처한 정확한 session/runtime/epoch 대상은 유지한다.
3. 세션이 시작 중이면 전송을 명시적으로 준비 대기시킨다. 대상 확인 후 원래 사용자 요청을 한 번 전달한다. 대상이 닫히거나 교체되면 원문을 보존하고 그 요청만 종료한다.
4. runtime queue 수락과 CLI 실행 확인을 구분한다. 과거 요청의 late ACK가 새 요청·새 편집 내용을 지우거나 새로운 자동 전송을 만들면 안 된다.
5. 실제 네이티브 편집기의 미제출 글자가 있는 경우의 동작은 명확히 정의한다. 상태 bool만 보고 차단하거나 기존 글자 뒤에 하단 본문을 몰래 붙이지 않는다. 검증된 non-text controls와 실제 editor 상태를 구분한다.

## PR 순서와 소유 범위

| PR | 목적 | 주요 파일 | 의존 |
| --- | --- | --- | --- |
| PR1 | 전송 단계·거절 원인 연계와 시작 재현 trace | runtime/input_admission.rs, runtime/in_process.rs, app/app.rs, 진단 타입·문서 | 없음 |
| PR2 | 오래된/잘못된 초안 증거로 새 전송이 막히는 결함 수정 | session/status.rs, runtime/input_admission.rs, runtime/in_process.rs, app/pty_effort.rs 및 조절 호출부 | PR1 |
| PR3 | 세션 시작 직후 준비 대기와 독립된 composer 요청 큐 | app/app.rs, app/agent_detect*.rs, app/ui/composer.rs, app controller 타입 | PR1·PR2 |
| PR4 | 새 작성·전송 계약과 전체 입력 경로 harness | app/ui/composer.rs, app/composer_drafts.rs, app/app.rs, 입력 harness·QA | PR1–PR3 |

공유 app.rs·composer.rs가 있으므로 구현 허가 후에는 위 순서로 경계 계약을 확정한다. 향후 병렬 구현을 요청받으면 app.rs 오케스트레이터 한 명, runtime/session PR2 한 명, 독립 fixture/harness 한 명으로 파일 소유를 나누고 같은 함수의 동시 편집을 피한다. 이번 조사에서는 서브에이전트를 새로 띄우지 않았다.

## PR1 — 내용 없는 연계 진단과 사건 trace

목표: 「왜 거절했는가」뿐 아니라 어느 요청·세션·입력 intent가 dirty를 만들었는지 확인할 수 있다. 이번 사건의 최초 유발 입력은 기존 로그만으로 미확정이므로 이 PR을 먼저 진행한다.

변경 범위:

- UI submit → controller → checkpoint → readiness → guard → PTY reservation → ACK의 동일 request ID 연결.
- 거절 이유를 bool/일괄 AdmissionDenied 외의 내부 enum으로 유지한다. 현행 public/wire 결과 호환성은 유지하거나 필요한 경우 별도 protocol 변경을 명시한다.
- session/runtime instance/CLI execution epoch/intent/row 분류(Empty·Placeholder·Draft·Unknown), 마지막 draft 원인 종류와 경과 시간만 bounded 진단에 기록한다.
- non-text control, native text, append, prompt submission을 입력 생산 지점부터 구분한다. 원문/terminal cells/경로/credential은 진단 로그에 넣지 않는다.
- UI에서 구체적 거절·준비 상태를 입력창 내부에만 표시한다. routine 저장 문구를 창 밖에 다시 띄우지 않는다.

수용 기준:

- fresh start + 즉시 전송, non-text control 후 전송, dirty rejection, Unknown/late ACK를 하나의 ID로 확인한다.
- 승인/질문/foreground 변경/준비 미완료/queue pressure를 서로 구분한다.
- per-session 이벤트·byte·시간 retention에 상한이 있고 idle 시 새 polling을 추가하지 않는다.
- 실제 발생 사건이 다시 관찰되면 마지막 dirty 원인까지 연계한다. 이번 기록에 없는 과거 조작은 추정으로 채우지 않는다.

검증: 순서 trace 및 내용 비노출 검사. 요청 ID A의 거절이 session B 또는 후속 ID C에 묶이지 않아야 한다.

## PR2 — 정확한 입력 intent와 현재 네이티브 편집기 판정

목표: 글자를 입력하지 않는 Codex 설정 키나 이미 지워진 초안 때문에 새로운 ExplicitPrompt가 막히지 않는다.

변경 범위:

- 수동/예약 강도 조절에서 같은 verified Codex non-text intent를 사용한다. 현재 AutomaticPrompt 한 경로에만 있는 바이트 예외를 생산 경로 계약으로 옮긴다.
- 실제 history Up/Down, 일반 텍스트, paste는 별도로 유지한다. A/B 최종 바이트 전체를 일괄 무시하지 않는다.
- Ctrl+U/backspace 후 신선한 editor 증거가 Empty 또는 **실제로 placeholder인 Placeholder**인지 확인한다. 현재 Empty만 허용하는 차이를 해결한다.
- old dirty 증거를 CLI 실행 epoch와 연결하여 PTY 안의 실행 전환 시 현재 실행에 맞게 재검증한다. epoch 변화만으로 실제 남은 글자를 삭제하거나 무조건 허용하지 않는다.
- native multi-line editor의 전체 초안 영역을 확인한다. 첫 행만 빈 것으로 보고 나머지를 덮어쓰지 않는다.

수용 기준:

- 이번 임시 재현의 manual Codex effort → redraw → Empty/Placeholder → 새 explicit prompt 사례가 전달된다.
- Ctrl+U → 현재 Codex placeholder 복원 → 새 prompt 전달.
- 실제 초안, choice dialog, 승인 요청, 잘못된 foreground, stale execution의 기존 입력 보호가 유지된다.
- 자동 broadcast/followup과 사용자의 새 작성·전송을 같은 규칙으로 무조건 열지 않는다.

검증: isolated 실제 PTY + production guard trace로 전송된 byte 수·session·request ID를 확인한다. 입력 거절을 단순 성공 결과로 바꾸는 패치는 수용하지 않는다.

## PR3 — 시작 준비와 composer 전송 요청 분리

목표: session target 생성 후 즉시 작성·전송해도 감지가 끝나기 전이라는 이유로 첫 요청이 버려지지 않는다.

변경 범위:

- composer를 `Editable`과 request delivery state로 분리한다. Preparing/Queued/Admitted/Rejected/Unknown은 제출 요청별 상태다.
- UnconfirmedAi context를 영구 캡처하여 즉시 거절하는 대신 정확한 target/generation을 고정하고 준비 대기시킨다.
- 동일 target의 실행 감지 + foreground/modes가 확인되면 준비된 실행 identity로 실제 전송을 한 번 시도한다. 다른 새 세션으로 fallback하지 않는다.
- single controller slot 사용 중이면 별도 bounded composer intent 큐에 요청을 유지한다. 기존 controller 동작을 덮어쓰지 않는다.
- draft 저장 checkpoint 실패/timeout/CLI 시작 실패는 그 요청의 상태로 정리하고 작성 원문을 유지한다.
- 준비 대기는 polling flood 없이 관련 readiness/controller/save completion 이벤트와 bounded timeout으로 깨운다.

수용 기준:

- start → 0/10/50/200ms에 사용자 작성·전송 → 감지 완료: 지정 세션에 본문+Enter 한 번.
- controller slot이 세션 시작 요청으로 occupied여도 이후 준비되면 한 번 전달.
- 기다리는 동안 내용 수정·다른 세션 전환이 원래 요청 target/body를 바꾸지 않는다.
- close, restore replacement, CLI crash, sleep/wake, deadline/late detector result에 다른 세션으로 전송되지 않는다.
- 앱/worker/render를 blocking wait로 멈추게 만들지 않는다.

검증: launch/ready/submit의 순서를 바꾸는 App → runtime trace. 설정된 종류 감지 주기(현재 기본 kinds1,200ms)의 sleep으로 덮는 방식은 수용하지 않는다.

## PR4 — 새 요청은 독립적이라는 계약과 harness

목표: 같은 본문 재입력과 이전 Unknown 결과가 이후 독립된 새 입력을 부당하게 잠그지 않는다. 다중 세션에서도 요청·초안·ACK가 섞이지 않는다.

변경 범위:

- edit revision/draft generation과 submission/operation identity를 분리한다. 매 새 전송에 새 ID를 만든다.
- pending 또는 Unknown인 이전 delivery의 retention과 후속 작성 buffer를 분리한다. 과거 결과는 과거 요청에만 표시·정산한다.
- 사용자가 다시 작성하여 전송하는 새 요청과 같은 미확인 operation의 자동 retry를 구분한다. 자동 retry를 만들지 않는다.
- 같은 내용의 새 요청을 과거 history 내용으로 합치지 않는다. history UI의 중복 표시 정책과 실제 delivery semantics도 분리한다.
- composer focus/IME/startup/controller/guard/ACK 전부를 한 ordered harness로 연결한다. 현재 terminal IME harness만으로 하단 composer startup 전송을 검증했다고 취급하지 않는다.

필수 시나리오:

| 축 | 사례 | 확인 |
| --- | --- | --- |
| 새 요청 | 동일 문장 2회, 거절 뒤 재작성, 이전 Unknown 뒤 독립 작성 | 새 request ID·정확한 세션·각 1회 |
| 시작 | fresh/resume/manual CLI 시작, ready 전/후 submit | 초안 유지·정확한 준비 전이 |
| provider | Claude/Codex/Kimi/Grok, 수동 실행 Cursor 등 Other | 확인 가능한 각 CLI 입력 계약 별도 fixture |
| 편집기 | Empty/Placeholder/실제 다중행 draft/choice/approval | actual input과 old evidence 구분 |
| 길이·입력 | 한글 IME/commit+Enter/한글 경로 paste/NFC/긴 문장/멀티라인 | 원문 보존·paste와 submit 순서 |
| 상태 | controller busy/checkpoint slow/queue pressure/reject/late ACK | 결과는 원래 요청만 정산 |
| 대상 | 같은 폴더 다중 세션/분할/다른 workspace/close/epoch 교체 | ID 교차 전송 없음 |
| 수명 | sleep/wake/reconnect/runtime retirement/unknown receipt | 교착·임의 자동 전송 없음 |

현재 실제 재현은 Codex 및 fake PTY의 제품 guard에 한정한다. 모든 provider live QA 완료를 주장하지 않는다. 구현 단계에서 fixture와 실제 CLI 버전을 기록하고, live 앱 테스트는 그 작업의 명시적인 재실행·시험 전송 허가 범위에 따라 진행한다.

## 현재 실행 결과와 다음 명령

이번 조사의 제품 모듈 detector/guard 재현 exit0, 기존 runtime 보호 회귀7PASS/0FAIL. 이 계획의 PR1–PR4 기능은 아직 구현하거나 테스트하지 않았다.

정확한 read-only 확인:

```sh
cd /Users/jr/Desktop/projects/deppy-sijo-performance
git status --short
cat docs/qa/2026-10-11-composer-startup-input-investigation.md
cat /private/tmp/deppy-composer-startup-investigation-jop_yiyy/guard-probe.log
cat /private/tmp/deppy-composer-startup-investigation-jop_yiyy/runtime-existing-results.json
git diff --check
```

향후 구현 검증 후보(아직 이 명령으로 새 테스트를 실행하지 않음):

```sh
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -p session pr2_draft_evidence_survives_redraw_turn_hook_and_bracketed_newlines
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -p runtime composer_ -- --test-threads=1
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -p deppy-sijo --bin deppy-sijo pr1_busy_host_slot_does_not_erase_draft_or_history
python3 /private/tmp/deppy-audit-nine-pr-20261004/cargo_gate.py test -p deppy-sijo --bin deppy-sijo ime_input_harness
```

PR별 source-only 검증 후 배포 요청이 있으면 그때 마지막 shipped0.10.1보다 높은 버전과 artifact 검증이 필요하다. 이번 조사에는 release/restart가 포함되지 않는다.
