# Cursor 사용량 상태바 설계

## 목표

설치되고 활성화된 Cursor CLI 계정의 실제 구독 사용량을 터미널 하단 provider 상태바에
표시한다. Cursor 개인 플랜은 주간 창을 제공하지 않고 월간 결제 주기 사용량을 제공하므로,
주간 값처럼 바꾸지 않고 `월 N%`로 표시한다.

## 데이터 원천과 보안 경계

- 런처가 감지한 정확한 `cursor-agent` 실행 파일만 사용한다.
- 사용자 프로젝트와 분리된 `~/.deppy-sijo/usage-probe`에서 공식 CLI를 PTY로 실행하고
  `/usage` 명령을 입력한다.
- Cursor 토큰, SQLite 데이터베이스, 설정 파일, 비공개 HTTP/RPC 계약은 읽지 않는다.
- 출력은 100 KiB로 제한하고 25초 안에 종료한다. 프로브는 백그라운드에서 실행하며
  마지막 성공 값은 기존 provider와 같은 freshness 계약을 따른다.
- 계정 사용량은 월 단위로 급격히 바뀌지 않으므로 5분마다 갱신한다.

## 데이터 모델

`CursorUsage`는 상태바에 필요한 값만 보존한다.

- `included_percent_used: u8`
- `auto_percent_used: Option<u8>`
- `api_percent_used: Option<u8>`
- `plan_name: Option<String>`
- `reset_label: Option<String>`
- `on_demand_enabled: Option<bool>`

퍼센트는 0~100으로 제한하고, 표시 문자열은 길이 상한을 적용한다. 패널이 완성되기 전의
임시 프레임보다 마지막으로 그려진 값을 선택한다.

## 화면

- 전체 폭: Cursor 로고와 `월 N%`를 표시한다.
- 좁은 폭: 다른 provider처럼 `N%`만 표시한다.
- 호버: 플랜, 월간 Included, Auto, API, 초기화일, On-Demand 상태를 가능한 항목만 합쳐
  보여준다.
- 감지됐지만 아직 조회 중이거나 조회에 실패한 경우 `—` 자리를 유지한다.
- 런처에서 Cursor를 끄면 프로브와 상태바 칸을 모두 숨긴다.

## 실패 처리

CLI 미설치, 로그아웃, 화면 계약 변경, 입력 backpressure, timeout은 앱을 실패시키지 않는다.
현재 프로브만 실패로 끝내며 다음 갱신 주기에 다시 시도한다. 수치가 없는 상태를 0%로
만들지 않는다.

