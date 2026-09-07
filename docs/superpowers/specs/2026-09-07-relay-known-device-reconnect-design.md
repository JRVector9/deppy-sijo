# Relay 알려진 기기 재접속 설계

## 목표와 승인 범위

PR #146의 최초 페어링 이후 URL 없는 재접속을 완성한다. 앱 재빌드·재실행,
force-push·rebase는 하지 않는다. DNS/TLS·외부 배포·실기기·24시간 soak는 BLOCKED다.

## 신뢰와 수명

기존 페어링 ticket은 300초·1회용을 유지한다. 별도 reconnect grant는 브라우저가
생성하는 32바이트 난수이며 자원 입장만 허용한다. 실제 인증은 기존 서명된 새 ephemeral
handshake, Mac에 저장된 기기 공개키, 브라우저에 고정한 Mac fingerprint로 한다.
raw grant는 브라우저 IndexedDB에만 영속 저장한다. Mac은 SHA-256 verifier만 받는다.
Relay도 verifier만 게시받고 입장 때 제시된 grant의 SHA-256을 대조한다.
Relay 서버의 SHA-256 의존성은 입장 검증자 계산에만 사용하며 ECDH/HKDF/AEAD와
애플리케이션 평문은 계속 서버 경계 밖에 둔다.

기존 80바이트 pairing URL에는 route가 없다. 최초 DeviceAdmission의 ZERO route만
최대 1024개 route·각 8개 ticket에서 유일한 유효 ticket으로 해석한다. 일치가 없거나
둘 이상이면 거절한다. 명시적 route와 reconnect admission에는 이 탐색을 적용하지 않는다.

## wire와 등록 순서

- DRLY 0x14 ReconnectPublish: verifier 32바이트 + expires_at u64 big endian.
- DRLY 0x15 ReconnectRevoke: verifier 32바이트.
- DRLY 0x16 ReconnectAdmission: raw grant 32바이트.
- DRLY 0x17 ReconnectSync: 빈 payload, Mac의 DB verifier 복원 완료 신호.
- DRLY 0x24 ReconnectPublished: verifier 32바이트 게시 ACK.
- 최대 grant 수는 라우트당 64개, 게시 가능한 잔여 수명은 최대 30일이다.
  grant 만료는 device 만료를 넘지 않고 같은 verifier 재게시로 수명을 늘리지 않는다.
- 최초 SAS 승인과 DB commit 후 Mac은 암호 `relay_registered`로 device_id, route_id,
  expires_at을 전달한다. fingerprint는 현재 검증한 hello에서 얻는다.
- 브라우저는 grant를 생성해 암호 `relay_register`로 verifier만 제출한다.
- Mac은 활성 principal을 재확인하고 verifier를 DB에 저장한 뒤 Relay에 게시한다.
- 게시 ACK 후 Mac은 암호 `relay_ready`를 보낸다. 브라우저는 identity fingerprint,
  Mac fingerprint, route, device id, grant, expiry를 한 IndexedDB transaction에 저장하고
  transaction complete 이후 세션 화면을 활성화한다.
- 재접속은 등록 레코드로 입장하고 Mac pin을 검증한 뒤 KnownDevice를 보낸다.
  Mac은 저장된 기기 키·취소·만료를 검사하고 암호 `relay_ready` 뒤 화면을 보낸다.

## 실패·재시작·취소

Mac은 Relay 입장 성공마다 유효한 verifier를 재게시하고 ReconnectSync로 끝을 표시한다.
새 route는 복원 중이며 이때 reconnect admission은 RouteBusy로 제한 재시도한다.
복원 완료 뒤에만 없는 grant를 CredentialRejected로 거절하므로 재시작을 회수로 오인하지 않는다.
grant 게시·취소는 라우트별로
제한되고 악성 중계자는 데이터 권한을 얻지 못한다. 활성 principal은 명령 수신과 화면
송신 전에 repository에서 다시 검사한다. 취소 UI의 worker 재시작은 유지한다.
만료·취소·인증 실패는 자동 재시도를 중단한다. 저장 실패는 성공으로 숨기지 않는다.
새 pairing URL은 기존 등록보다 우선한다. 이전 세대의 async 결과와 socket 이벤트는
현재 화면·저장소를 바꾸지 못한다. 네트워크 재시도는 횟수와 backoff가 제한된다.

## 검증 경계

wire 크기·오용, 반복 재접속·만료·취소·상한, SQLite restart, Mac pin 불일치,
IndexedDB commit 실패, 새 링크 우선, stale async 결과를 RED→GREEN으로 검증한다.
실제 배포 artifact digest·DNS/TLS/CSP·실기기·24시간 soak는 별도 BLOCKED이며
로컬 테스트 결과로 대체하지 않는다.
