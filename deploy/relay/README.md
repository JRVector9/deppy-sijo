# Deppy Relay 데이터 평면 배포

이 디렉터리는 **신뢰하지 않는** Relay 데이터 평면(`crates/relay-server`)의 배포 매니페스트를
담는다. 신뢰하는 모바일 셸은 여기서 서비스하지 않는다 — 다른 오리진의 몫이다.

## 신뢰 모델 요약

- Relay는 암호문만 중계한다. 복호화에 필요한 코드가 애초에 링크되지 않는다
  (`relay-server`의 의존성 법칙 테스트가 고정한다).
- Relay가 알 수 있는 것: 프로토콜 버전, 불투명 라우트/연결 핸들, 시퀀스, 길이, 생존 신호.
- E2EE 이전의 서명된 hello는 최대 512바이트 불투명 레코드 하나이며 그대로 전달된다.
  **애플리케이션 평문**은 어떤 프레임에도 없다.
- Relay는 HTML·JavaScript·서비스워커·WebAssembly를 절대 서비스하지 않는다.
- TLS는 배포 edge가 종단한다. 프로세스 자신은 평문 TCP 위 WebSocket만 말하며 공개 노출은
  edge를 통해서만 이뤄져야 한다.
- v1은 **단일 인스턴스**다. 라우트/티켓/연결 상태를 공유 저장소에 두지 않으므로 인스턴스를
  늘리면 페어링이 깨진다.

## 현재 상태: BLOCKED — PASS 아님

아래 좌표가 정해지기 전까지 스테이징/프로덕션 publish·deploy는 **차단**이다. 로컬 프로토콜·
서버 작업은 이 좌표를 지어내지 않고 계속 진행할 수 있다.

| 항목 | 상태 | 필요한 결정 |
|---|---|---|
| Mac 승인 자격증명 provisioning/회전 주체 | BLOCKED | 누가 발급·회전·폐기하는가 |
| 스테이징/프로덕션 도메인 | BLOCKED | 정확한 호스트명 |
| DNS 소유자 | BLOCKED | 레코드 관리 주체 |
| TLS edge | BLOCKED | 종단 지점과 인증서 발급/갱신 주체 |
| 신뢰 Origin | BLOCKED | 셸 오리진 상수(모바일 셸 CSP와 공유) |
| 컨테이너/아티팩트 레지스트리 | BLOCKED | 위치와 접근 권한 |
| GitHub 환경·시크릿 이름 | BLOCKED | 환경명과 시크릿 키 |
| 배포 자격증명 | BLOCKED | 발급 주체 |

`*.env.example`의 값은 전부 자리표시자다. **기본 자격증명은 존재하지 않는다** — 기본값이
있는 순간 그것이 곧 백도어다. `relay-server`는 `DEPPY_RELAY_ROUTES` 없이 기동을 거부한다.

## 매니페스트

- `staging/relay.env.example`, `production/relay.env.example`: 필요한 환경변수 전체 목록.
- `staging/relay-server.service`, `production/relay-server.service`: systemd 유닛.

두 환경은 **독립적으로 롤백 가능해야** 한다. 한쪽 롤백이 다른 쪽 아티팩트나 자격증명을
건드리면 안 된다.
