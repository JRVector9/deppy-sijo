# Relay 앱 어댑터 이관 계약

R3는 R2 `feat/relay-shell-main` 위에 앱의 Relay 활성화만 연결한다. R1 서버·저장소와 R2 브라우저 셸을 선행 의존성으로 둔다.

## 앱 연결

- Tailscale과 Relay 설정은 독립적으로 켜고 끈다. 두 경로는 SessionCore를 공유한다.
- 설정 렌더는 typed intent만 만든다. DB 작업과 네트워크 연결, 키 접근은 controller 또는 worker가 수행한다.
- AppRelayRepository는 single-instance lock을 보존하고 승인·회수·재접속 verifier를 저장소에 전달한다. raw reconnect grant는 브라우저에만 남으며 DB·로그·config에 저장하지 않는다.
- 신규 Relay identity supplier를 만들거나 Relay가 OFF인 worker를 시작할 때 Keychain 호출은 없다. 실제 handshake에서 공급자를 요청할 때 키를 조회·생성한다. counting SecretStore 테스트는 호출 0과 동일 공급자를 명시적으로 실행하는 양성 대조를 모두 검사한다.

## 승인 세대와 v39 마이그레이션

`authorization_epoch`는 16바이트 불투명 승인 세대다. 인증 토큰이나 비밀이 아니며 네트워크 자격증명으로 사용하지 않는다. 승인 트랜잭션마다 새로운 UUID를 발급하므로 같은 초에 같은 기기를 재승인하거나 삭제 후 재생성해도 이전 채널과 구분한다.

기존 v38 행에는 마이그레이션 한 번만 무작위 세대를 부여한다. 기기 ID·공개키·권한·발급 시각·만료·회수 여부와 reconnect verifier를 변경하지 않는다. DB 마이그레이션은 Relay worker 생성 전에 끝나고, 이후 handshake는 저장된 세대를 읽는다. 기존 기기를 회수하거나 재페어링하도록 만들지 않는다. 재시작 시 세대도 그대로 유지한다.

활성 principal은 송신과 수신 경계에서 현재 저장소의 세대, 공개키, 권한, 발급 기간 및 유효성을 다시 검사한다. 재승인 후 이전 principal은 채널을 닫는다. 충돌 또는 승인 도중 SQL 실패는 전체 트랜잭션을 롤백하여 기존 세대와 verifier를 보존한다.

## 후속 작업과 검증 경계

기존 OAuth `App::new` 시작 reconciliation은 이 PR에서 변경하지 않는다. 앱 시작의 반복 Keychain 팝업은 별도 main 기반 R4 `fix/keychain-startup-lazy`에서 해결한다. 이 PR의 Relay 신규 경로 호출 0 결과를 앱 전체 시작 호출 0으로 해석하면 안 된다.

fleet·file-tree·IME·font·일반 UI, 기존 secret crate 정책, 일반 패키징·공증·xtask는 이관하지 않았다. 앱 빌드·재실행과 외부 배포도 수행하지 않았다. DNS·TLS·자격증명·실기기·24시간 soak는 BLOCKED다.

R2의 GitGuardian incident 37016215는 `relay-hello-v1.json`의 공개 순차 바이트 테스트 벡터다. R3는 해당 fixture를 새로 추가하거나 탐지 문자열을 변경하지 않는다. 외부 검사 실패를 로컬 테스트 PASS로 대체하지 않는다.
