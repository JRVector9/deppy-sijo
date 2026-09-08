# Keychain 시작 접근 지연

앱 생성자는 기존 자격증명 정리와 Codex LLM API 키 존재 확인을 수행하지 않는다. Settings 및 Agent Sessions 화면 렌더도 Keychain을 조회하지 않는다. 저장 여부를 모르는 API 키 상태는 별도로 표현하며, 명시적 저장·삭제 버튼은 사용할 수 있다.

## 지연 복구 경계

시작할 때는 DB의 bounded ledger snapshot만 캡처한다. v38 `recovery_generation`은 각 ledger 행의 생성 세대이며 비밀이나 인증 토큰이 아니다. 기존 행은 한 번 백필하고 INSERT 시마다 새 세대를 발급한다. 시간 기반 추정으로 복구 대상을 고르지 않는다.

명시적 credential/agent 실행/Connector 작업이 복구를 요청한다. 기본 후보는 시작 snapshot의 exact slot+generation이며 다른 worker가 새로 만든 Staging 슬롯은 추가하지 않는다. 복구 함수가 직접 등록한 이관 슬롯은 실패 시에만 동일한 bounded pending 목록에 보존하여 재시작 없이 재시도한다.

복구는 해당 세대의 현재 상태를 다시 읽고 SQLite IMMEDIATE 트랜잭션 안에서 generation·state·참조를 비교한다. Keychain cleanup callback과 DB ack까지 같은 보호 범위에 둔다. Published access 슬롯은 삭제하지 않으며 남은 legacy username만 정리한다. 실패하면 DB 의무는 남고 다음 명시적 작업에서 다시 시도한다. callback은 DB에 재진입하지 않는다.

legacy access/refresh/DCR은 기존 물리 슬롯 publish CAS로 이관한다. 오류가 나도 유효한 physical credential 사용을 일괄 차단하지 않으며, logical pointer fallback은 계속 거부한다. Keychain 값·식별자·오류 원문을 새 로그에 넣지 않는다.

## 최초 Connector 요청

자기 요청의 이관이 revision을 올린 경우만 내부 재평가를 한 번 허용한다. App adapter는 시작 revision + 성공한 publish CAS 개수와 종료 revision이 같고 현재 revision도 여전히 같음을 확인한 뒤 증명을 한 번 소비한다. Coordinator는 외부 부수효과 전에 overview를 갱신하고 같은 target을 한 번 다시 읽는다. 다른 writer의 변경을 자동 승인하지 않으며 기존 stale 거절을 유지한다.

## 검증 및 착지

테스트는 실제 App::new 및 제품 KeyringSecretStore에 counting mock을 연결한다. 생성·Relay OFF·Settings/Agent Sessions 렌더의 접근0, 명시적 저장·삭제/legacy 사용 양성대조, UI 버튼 intent, 실패 후 재시도, 신규 Staging 보존, DB 재생성 ABA와 publish 경합, backfill/reopen을 확인한다. Connector는 첫 클릭 실행과 외부 writer 개입 시 실행0을 검증한다. 실행 결과는 CODEX_HANDOFF와 PR에 기록한다.

이 PR은 main 독립 v38이며 Relay #161의 v38 및 #163의 v39와 migration 번호가 병렬 충돌한다. 어느 PR이 먼저 착지하든 나중 PR에서 최신 main을 일반 merge하고 migration을 뒤 번호로 재배치한 뒤 전체 migration 테스트를 다시 실행해야 한다. force-push/rebase는 사용하지 않는다.

앱 빌드·재실행과 실환경 Keychain 대화상자 시각 확인은 수행하지 않았다. DNS·TLS·배포·운영 자격증명·실기기는 이 작업에서 검증하지 않는다.
