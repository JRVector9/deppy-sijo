# Relay 비UI 코어 main 이관 계약

이 변경은 PR #146 head `75bf2c9`의 Relay 코어를 `origin/main 45e66cc`에 hunk별로 이관한다. 앱에서 Relay를 켜거나 기기 연결을 완료한 상태를 의미하지 않는다.

## R1 포함

DRLY 재접속 프레임과 서버 입장 검증, signed ephemeral handshake, 기기 키와 Mac fingerprint 고정, verifier의 SQLite v38 저장·재시작·회수, bounded worker 수명주기.

R2 `feat/relay-shell-main`은 R1을 base로 브라우저 IndexedDB 등록 commit/세대 처리, shared viewer, 로컬 artifact 무결성과 Chrome/Node fixture를 추가한다. 신규 JSON을 읽는 Rust 교차 언어 벡터 테스트도 R2에 속하며 R1은 그 fixture 없이 컴파일된다.

raw reconnect grant는 브라우저가 생성해 IndexedDB에 저장한다. Mac과 DB는 SHA-256 verifier만 취급한다. Relay 입장은 자원 접근만 허용하며 종단 데이터 권한은 새 핸드셰이크와 repository 재검증으로 별도로 판정한다. 최초 페어링 ticket은 300초·1회용이며 재접속 grant는 라우트당 64개·최대 30일을 넘지 않는다.

## 후속 app adapter PR

`crates/app/src/relay_repository.rs`의 `AppRelayRepository`가 reconnect store/read 메서드를 구현하고, app relay pairing/worker lifecycle/UI를 연결해야 한다. 이번 기본 repository 메서드는 미구현 시 실패하므로 불완전 어댑터가 성공을 가장하지 않는다.

storage `RelayPendingInsert::Conflict`와 그 repository 매핑은 기존 app의 exhaustive match를 깨므로 이관하지 않았다. 같은 기기 pending 교체, 동일 키 재페어링 행 교체, 회수 행 수거 정책도 후속 app adapter 변경과 함께 검증한다. 기존 enum/페어링 정책은 이번 PR에서 유지한다.

secret macOS keychain 확대, 앱 packaging/notarization과 관련 xtask, 앱 실행용 relay-dev.sh, native app/UI/settings/i18n/fleet/file-tree/font/IME는 제외한다. main #155의 push.rs와 ureq/HTTP 의존성 버전도 유지한다.

## GitGuardian 공개 테스트 벡터 근거

2026-09-08 GitHub check-run `101599401791`의 output.text는 incident `37016215`를 `crates/web-remote/tests/fixtures/relay-hello-v1.json:35`(원본 commit `53f2a31`)로 지목한다. 탐지 유형은 Generic High Entropy Secret이다.

해당 파일은 R2에만 포함된다. 그 값은 `pairing_proof.secret_hex`의 순차 바이트 0x00부터 0x1f까지로, 독립 인코더의 공개 결정론 테스트 벡터다. 파일 note가 production key material이 없음을 명시한다. 소비 경로는 cfg(test) handshake와 Chrome 벡터 fixture이고, `web/relay-shell/build.sh`의 배포 파일 목록에는 tests/fixtures가 없다. 프로덕션 route/admission 상수도 None이다. 실제 자격증명·배포 키·재사용 가능한 운영 grant를 복사하지 않았다.

벡터 원문을 유지하며 문자열 분할, scanner 억제 설정, 원본 이력 수정으로 탐지를 우회하지 않는다. GitGuardian의 최종 상태는 로컬 기능 검사 결과와 별도로 기록한다.

## 검증 경계

로컬 Rust/Node 테스트와 `.test` 오리진으로 생성한 artifact의 자체 digest 검증은 실행 가능한 개발 검증이다. 실제 DNS/TLS, 배포 자격증명, 외부 배포, 실기기 접속, 24시간 soak는 모두 **BLOCKED**다. 이를 로컬 PASS로 대체하지 않는다. 앱 빌드·재실행도 하지 않는다.
