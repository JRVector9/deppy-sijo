# 프로젝트 키 관리 구현 계약

기준: ff722082 / 0.10.3. 사용자 승인: 로컬 최신화, 보고서의 전체 기능 및 HTML 시안 구현, 구현 후 누락 재점검. 실제 사용자 키/파일/GitHub 변경·앱 재시작은 테스트에 포함하지 않는다. 격리된 fake store/임시 DB/원본/fake remote를 사용한다.

## 완료 기준

- [x] PR1: 환경파일·수동 API 통합 목록, 원본 표시, 명시적 수정, 숨김 값 더블클릭 및 버튼 복사. 실제 clipboard intent, 프로젝트/revision fencing, 빈 수정값 유지, 실패 안내.
- [x] PR2: 개발/운영 scope, dotenv와 PEM 파일 원본/앱 보관 원본 구분, 활성 화면과 무관한 bounded 감시·재시작/잠자기 복귀 재검사, last-good 유지·누락·충돌 표시, metadata 원장.
- [x] PR3: 저장 가능한 빌드 명령, CLI wrapper/앱 연결 실행, 최신 원본 preflight, 실행 snapshot 고정, 사용 이름·버전·시각 기록. 프로젝트 프로그램/agent 인증 scope 보존.
- [x] PR4: GitHub 인증 credential과 정확한 Repository/Environment/Organization 목적지 연결, 암호화 전송, 자동 전송 opt-in, durable 재시도 큐·remote 최신 revision fencing, 외부 변경 시 값 미확인/자동 전송 보류.
- [x] PR5: storage/worker/transport/UI end-to-end 회귀·보안·격리·실패/순서/재시작 테스트, 시안의 표/상세/모달/버튼 규격 대조, 코드 리뷰 결과 수정, 문서/일지/커밋.

## 파일 소유권

1. domain_storage: `crates/core/src/project_keys.rs`, core lib export, `crates/storage/src/project_keys.rs`, storage module/schema/export. 순수 metadata DTO 및 Db APIs와 tests. 제품 secret 값은 DTO/SQLite/Debug로 전달하지 않는다.
2. key_backend: 새 `crates/app/src/project_keys/` 서비스/worker/IO, `crates/app/src/app.rs`, `main.rs` 및 필요한 CLI entry. UI 화면은 수정하지 않는다. App root 통합은 이 에이전트만 맡는다. 기존 secret bundle CAS/ledger 사용.
3. key_ui: 새 `crates/app/src/ui/project_keys.rs`와 필요 하위 UI 모듈, `ui/mod.rs`, 지역화 catalog, 기존 환경/UI의 필요한 adapter. app.rs/main.rs/storage/core/Cargo 파일은 수정하지 않는다.
4. root: 이 계획·handoff·report·공용 popup 사례 및 규칙·Cargo dependency/version/lock, 독립 CLI 리뷰·통합 tests·커밋. 소유 파일 변경은 소유자와 먼저 조율한다.

공통 타입의 최종 코드는 domain_storage가 작성하고 backend/UI에 즉시 알린다. UI/backend는 계약이 확정되기 전 adapter와 테스트 설계부터 진행한다. Cargo dependency 변경은 root에게 요청한다. git commit/rebase/switch/format 전체를 subagent가 실행하지 않는다. 소유 파일만 rustfmt한다. root가 PR 단위 커밋한다.

## 공통 모델 요구 사항

`deppy_core::project_keys`: 개발/운영 environment, source(Vault/Dotenv selected file/PEM selected path/GitHubOnly), kind(API token/client secret/private key/configuration), usage(build/store-only/explicit legacy agent), key metadata ID/workspace/name/credential reference/revision/timestamps/storage state, GitHub target(account credential reference + repository/organization identity + scope/environment/secret name + auto flag), remote state/last sent revision, build recipe/run receipt. 값은 없고 validate/limit helpers 제공.

DB: 키 metadata, 프로젝트/환경별 설정과 선택 source 목록, 빌드 recipe/receipt, GitHub target/outbox 상태. version migration은 append-only. 기존 env/profile/API metadata를 처음 조회 시 정확한 원본과 우선순위를 보여주고 기존 참조/인증 의미를 유지해 도입한다. 중복키는 임의 병합하지 않는다. bounded queries, ownership+environment 검사, CAS revision/fenced acknowledgement. legacy scope는 개발로 유지하고 운영에 복사하지 않는다.

worker: 네트워크/FS/Keychain/SQLite/child process를 UI render 밖에서 수행. bounded latest-only snapshots/정확한 mutation 결과; 모델에 실키 평문 포함 금지. secret 값은 non-Debug/non-Serialize sensitive buffer; async copy는 현재 프로젝트/키/revision 확인 후 OS clipboard 사용. 기본 숨김 유지. worker command와 snapshot 정의는 backend와 UI가 빠르게 맞춘다.

GitHub: CLI 인증 재사용 또는 저장된 token ref를 명시적으로 연결; Secret read는 metadata만. 공개키 조회 + sealed-box 암호화 전송 또는 동일 계약의 official gh secret set(stdin, argv/log에 secret 없음). scope 정확히 고정. fake transport로 실제 outbound 수행 API 테스트. GitHub App private key는 사용자가 지정한 PEM 원본/보관본, remote private key pull 기능으로 표현하지 않는다.

빌드: 이름/명령/cwd/env scope를 명시적으로 저장. CLI `deppy-sijo env run ...`의 실제 문법을 구현 후 문서화. 기존 shell 환경을 바꿨다고 주장하지 않는다. 작업 시작 전 선택 원본 새로 읽고 단일 snapshot을 build child에 전달. 임시 PEM 파일 제한 권한/종료 cleanup. 실제 nomorevibe build·agent restart는 실행하지 않는다.

UI: 승인 HTML `docs/mockups/project-keys-proposal-2026-10-12.html`에 맞춘 상단 프로젝트+환경+원본+감시, 공통44px rows, 목록28px buttons, 일반34px buttons,36px fields, 선택 상세300px, 수정 dialog480px+고정footer/내부scroll. 좁은 창은 상세 아래. 키 이름=상세, 값 더블클릭=복사, 명시적수정/복사 buttons. 추가/파일import/충돌해결/빌드사용/GitHub설정·재시도 모두 실제 intent에 연결한다. 영구 가짜 성공/disabled TODO 기능 금지. 기존 context-prefill/공유키/legacy advanced 기능 보존. 언어별 같은 키.

## 검증/리뷰

실행하고 결과를 기록한다: 관련 scoped tests → workspace check/xtask boundary → fmt/clippy → 전체 nextest(ci profile), 필요 unit doctests/PWA 회귀. root가 각 코드 diff를 `codex exec -m gpt-6.1-sol -c model_reasoning_effort=xhigh -s read-only ...`로 직접 리뷰하고 실제 지적을 수정한다. raw logs는 private tmp에, 비밀값 없는 보고서만 docs에 기록. 릴리스 전달 시 최신 버전 다시 확인하고 minor bump+bundle version 검사; 앱 재시작은 명시 요청 없으므로 실행하지 않는다.

## 재점검 표

각 완료 기준별 구현 위치/실행 test/결과/누락 항목을 `docs/qa/2026-10-12-project-key-management-acceptance.md`에 기록한다. 테스트가 없다면 PASS로 표시하지 않는다. 덜 구현된 항목은 다시 구현/검증한 뒤 종료한다.

## 최종 검증

2026-10-12: [수용표](../../qa/2026-10-12-project-key-management-acceptance.md)33/33행 PASS. 전체 nextest5366PASS/63skipped, normal check·strict lint·boundary·deps·fmt PASS, isolated 실제 UI PNG export1PASS. 개발/운영·원본·빌드·GitHub·기존 설정 호환을 재검토하고 발견된 결함을 회귀 검증했다. 실행 앱·사용자 실키는 변경하지 않았으며 릴리스 build/배포/재시작은 포함하지 않는다.
