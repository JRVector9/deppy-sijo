# 보안 체크리스트 (PR-22, 설계문서 §2.1 / §6.3 / §7 / §1.4)

각 정책이 코드 어디에서 강제되고 어떤 테스트가 검증하는지의 매핑.
새 PR에서 secret/credential/로그를 만지면 이 표를 갱신한다.

## 1. Secret 저장 경계 (§2.1, §6.3)

| 정책 | 강제 지점 | 검증 |
|---|---|---|
| secret은 keyring에만 저장 | `secret::KeyringSecretStore` (유일한 저장 경로) | `storage.rs::db_파일에_secret_평문이_없다` (파일 바이트 스캔) |
| SQLite에는 keyring 좌표/masked_hint만 | `credentials` 스키마 (값 컬럼 없음), `env_vars` CHECK(secret이면 plain_value NULL) | 스키마 CHECK + env roundtrip 테스트 |
| config(toml)에 secret 없음 | `Config` 구조에 secret 필드 자체가 없음 | 코드 구조 (필드 부재) |
| UI는 get_secret 호출 금지 | resolve는 runtime worker의 SpawnAgent 경로에서만 | `in_process.rs` SpawnAgent 테스트 |
| OAuth access token만 env 주입 가능 (blob/refresh 금지) | `auth::store_token` — access는 credential id, refresh는 `{id}.refresh` | `auth::access는_credential_id에_refresh는_별도_entry에` |
| insecure fallback 금지 (§1.4) | `secret::init_platform_store` — 미지원 플랫폼은 등록하지 않음 | 코드 경로 (mock store는 test 전용) |
| keyring 접근 직렬화 (§1.4) | `KEYRING_SERIAL` 전역 Mutex | 코드 경로 |

## 2. 로그 Redaction (§7)

| 정책 | 강제 지점 | 검증 |
|---|---|---|
| 로그는 redaction 후에만 디스크 | worker pump 콜백: `redact_chunk` → `append_output` | runtime 통합 테스트 (로그 3종 평문 부재) |
| 등록 시점: credential 저장 / SpawnAgent resolve / 시작 SeedRedaction / OAuth 완료 | 각 경로에서 `RedactionService::register`(+`register_json_fields`) | credentials/connectors/worker 경로 테스트 |
| 변형 corpus: chunk 경계 / ANSI 삽입 / base64 / URL(대·소) / form(+) / JSON-escape / \uXXXX | `redaction.rs::register` 변형 등록 | `redaction_corpus_변형_fixture` (변형×3 시나리오) |
| lookbehind carry ≥ 최대 secret 길이 | carry cap = max(16K, max_len×2) | `긴_secret_분할` 계열 테스트 |
| MCP stderr redaction | `mcp` transport stderr thread → StreamRedactor | mcp 통합 테스트 |
| audit input은 redacted만 저장 (§11.7) | `audit::record_audit` 2단 redaction, 평문 컬럼 없음 | audit 평문 부재 테스트 |
| OS 알림/로그 메시지에 secret 원문 금지 | tracing 호출부는 값 대신 id/길이만 기록 | 코드 리뷰 관행 (PR-15에서 원문 로깅 제거) |

## 3. Env Leak (§6.3)

| 정책 | 강제 지점 | 검증 |
|---|---|---|
| agent env는 선택된 plain + credential 참조 resolve만 주입 | `RuntimeCommand::SpawnAgent`(값 없음) → worker resolve | SpawnAgent env 주입 테스트 |
| 앱 프로세스 자체 env에 secret 없음 | secret은 keyring 상주, env로 올리지 않음 | 코드 구조 |
| production profile 보호 | `env_profiles.is_production` (§6.4) | env profile 테스트 |

## 4. MCP Permission (§11.7, PR-16/22)

| 정책 | 강제 지점 | 검증 |
|---|---|---|
| tool 스키마 변경 시 재승인 | `audit::PermissionPolicy::evaluate` — approved hash 불일치 → NeedsApproval | `스키마_변경_시_재승인` |
| 기형 hash는 영구 승인 불가 (fail-closed) | `apply_decision` 64-hex 검증 | `기형_schema_hash는_영구_승인이_되지_않는다` |
| 규칙 변경 시 승인 이력 무효화 | `set_rule`/`DenyAlways` | 정책 테스트 |
| stdout은 valid MCP만 / 위반 시 연결 폐기 | `mcp` transport validate_jsonrpc + violation poison | mcp 프로토콜 테스트 |
| schema_hash는 연결 테스트 시 기록 | connectors `tool_rows` → `audit::schema_hash` | connectors 단위 테스트 |

## 5. 원격 경계 (PR-19)

| 정책 | 강제 지점 | 검증 |
|---|---|---|
| localhost-only bind/attach | `RemoteRuntimeServer::serve`(127.0.0.1 고정) / `attach`(loopback 검사) | remote 테스트 |
| public remote 전 auth/capability 필수 | `remote.rs` 모듈 주석 (SpawnAgent 노출 경고) | v1+ 선행 조건으로 문서화 |

## 잔여 (후속 트랙)

- `input_encrypted_blob` (AEAD + keyring key id) — 선택 기능, 구조만 존재 (§11.7)
- Windows 경로 검증 일괄 (keyring / taskkill / MSI) — Windows 머신 확보 시
- redaction: 동일 secret의 부분 문자열(접두부만 출력된 경우)은 미커버 — §7 정책상 전체 패턴 매칭 기준
