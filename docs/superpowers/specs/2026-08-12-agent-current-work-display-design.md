# 에이전트 현재 업무 표시 설계

## 목표와 범위

사이드바 서비스 상태의 PTY 에이전트 행 첫 줄에 모델명이나 context 출력이 아니라 작업
맥락을 표시한다. Claude, Codex, Kimi에 동일하게 적용한다. App Server의 별도 에이전트
화면, 상태 판정, resume, 런처, 알림은 변경하지 않는다.

앱 번들 재빌드, 서명, 재실행과 화면 확인은 사용자가 별도로 요청할 때 진행한다.

## 표시 우선순위

첫 번째 비어 있지 않은 값을 사용한다.

1. 현재 턴에서 에이전트가 출력한 최신 응답/진행 요약
2. 현재 턴을 시작한 실제 사용자 지시
3. App이 계산한 프로젝트 표시명
4. 현재 cwd의 폴더명
5. 기존 상태별 중립 문구

터미널 viewport의 마지막 줄은 후보에서 제거한다. 모델 상태줄, `context: 0%`, 셸
프롬프트와 진행 애니메이션을 업무로 오인할 수 있기 때문이다. 모델, effort, context
비율은 기존 보조 정보 줄에 그대로 유지한다.

## 데이터 계약

기존 `last_agent_summary`는 현재 턴의 에이전트 응답/진행 요약으로 유지하고,
`user_instruction`을 `TranscriptState`와 `AgentDisplay`에 별도 추가한다. 두 필드는 기존
정규화 규칙을 공유한다.

- 한 줄로 공백을 정리한다.
- 기존 문자·바이트·항목 상한을 적용한다.
- 시스템/환경 주입, 로컬 명령, 도구 결과는 사용자 지시로 사용하지 않는다.
- 원문은 `Debug`에 기록하지 않는다.
- 새 사용자 턴 뒤 아직 에이전트 출력이 없으면 이전 턴의 agent 요약을 재사용하지 않는다.

## 공급자별 추출

- Claude: 최신 실제 `user.message.content`. `tool_result`, `<local-command...>`,
  `<command-name...>`, 시스템·환경 주입 메시지는 제외한다.
- Codex: 최신 `event_msg.payload.type == "user_message"`의 `payload.message`.
- Kimi: 최신 `turn.prompt.input[]` text 중 `origin.kind == "user"`인 것만 사용한다.
  `system_trigger`는 사용자 지시로 취급하지 않는다.

에이전트 응답/진행 요약은 기존 공급자별 추출 방식을 유지하되 현재 턴 경계를 지킨다.

## 프로젝트 폴백

렌더 시점에는 파일시스템을 조회하지 않는다. 기존 `SessionProjectNameSnapshot`의 정확한
session/cwd 프로젝트명을 우선 사용하고, 값이 없으면 이미 감지된 cwd에서 `file_name()`을
구한다. 둘 다 없을 때만 기존 상태별 문구로 내려간다.

## 유지되는 동작

- 오른쪽 실행 상태 레이블과 색상
- `[PTY] Provider · model · effort · ctx` 보조 줄
- transcript 바인딩과 감지 주기
- 상태 우선순위, 승인 알림, 세션 포커스, resume
- 기존 UI 크기와 배치

## 검증

- Claude/Codex/Kimi가 최신 실제 사용자 지시를 별도 추출한다.
- 새 턴에는 이전 agent 요약이 보이지 않고 사용자 지시가 보인다.
- agent 요약이 생기면 사용자 지시보다 우선한다.
- 둘 다 없으면 프로젝트명, 이어서 cwd 폴더명이 보인다.
- Kimi `system_trigger`와 Claude 내부 reminder를 사용자 지시로 오인하지 않는다.
- 터미널 마지막 줄은 첫 줄 선택 API에 전달되지 않는다.

