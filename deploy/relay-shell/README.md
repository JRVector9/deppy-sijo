# Deppy Relay 신뢰 셸 배포

이 디렉터리는 **신뢰하는** 모바일 셸(`web/relay-shell`)의 배포 매니페스트를 담는다.
신뢰하지 않는 데이터 평면(`crates/relay-server`)은 여기서 서비스하지 않는다 —
`deploy/relay/`가 그쪽 몫이고, 두 오리진은 **절대 같을 수 없다**.

## 신뢰 모델 요약

- 셸 오리진은 명시적인 보안 주체다. 여기서 서비스되는 코드가 평문 터미널·입력·승인을
  다루므로, 이 오리진과 그 릴리스 절차는 신뢰 모델 **안**에 있다.
- Relay 오리진은 암호문만 받는다. 실행 가능한 코드를 절대 서비스하지 않는다.
  셸의 CSP `connect-src`는 그 오리진 **하나만** 지명한다.
- 아티팩트는 내용 주소로 불변이다. `web/relay-shell/build.sh`가 tar.gz 하나와
  사이드카 매니페스트 하나를 만들고, `build.sh verify`가 아카이브·개별 파일·크립토
  모듈 다이제스트를 되대조한다.
- 브라우저가 실제로 쓰는 크립토는 `relay-crypto.js` **하나**다. Rust ↔ 브라우저 고정
  벡터 하네스(`crates/web-remote/tests/relay_webcrypto_vectors.rs`)가 사본이 아니라
  바로 그 모듈을 import한다.
- 서명 없는 다이제스트 매니페스트만으로는 **다운그레이드를 막지 못한다**. 옛 아티팩트도
  자기 다이제스트에는 맞기 때문이다. 최소 프로토콜 버전은 서비스워커 캐시 **바깥**에서
  강제되어야 하고, 롤백 정책은 캐시 동작과 독립적으로 검증되어야 한다.

## 현재 상태: BLOCKED — PASS 아님

아래 좌표가 정해지기 전까지 스테이징/프로덕션 publish·deploy는 **차단**이다. 로컬
빌드·검증·브라우저 게이트는 이 좌표를 지어내지 않고 계속 진행할 수 있다.

| 항목 | 상태 | 필요한 결정 |
|---|---|---|
| 셸 스테이징/프로덕션 도메인 | BLOCKED | 정확한 호스트명 |
| 셸 DNS 소유자 | BLOCKED | 레코드 관리 주체 |
| 셸 TLS edge | BLOCKED | 종단 지점과 인증서 발급/갱신 주체 |
| 정적 호스트/CDN | BLOCKED | 위치와 접근 권한, 캐시·불변 경로 규칙 |
| 응답 헤더 주입 지점 | BLOCKED | CSP·`Cache-Control`을 실제로 붙이는 계층 |
| 불변 아티팩트 레지스트리 | BLOCKED | 아카이브 보관 위치와 보존 정책 |
| GitHub 환경·시크릿 이름 | BLOCKED | 환경명과 시크릿 키 |
| 배포 자격증명 | BLOCKED | 발급 주체 |
| 신뢰 Relay 오리진 상수 | BLOCKED | `deploy/relay/README.md`와 **같은** 값 |

`*.env.example`의 오리진은 전부 RFC 2606/6761 예약 이름이다. `build.sh`가 그것을
자리표시자로 판정해 매니페스트에 `*_is_placeholder: true`로 남긴다 — 실제 도메인이
들어오면 저절로 `false`가 된다.

## 매니페스트

- `staging/shell.env.example`, `production/shell.env.example`: `build.sh`가 읽는
  환경변수 전체 목록.
- `staging/headers.conf`, `production/headers.conf`: 정적 호스트가 **반드시** 붙여야
  하는 응답 헤더. CSP는 문서 안 `<meta>`가 아니라 응답 헤더가 권위다.

두 환경은 **독립적으로 롤백 가능해야** 한다. 한쪽 롤백이 다른 쪽 아티팩트·오리진·
자격증명을 건드리면 안 된다. 그래서 두 환경은 오리진도 GitHub 환경 이름도 공유하지
않는다.
