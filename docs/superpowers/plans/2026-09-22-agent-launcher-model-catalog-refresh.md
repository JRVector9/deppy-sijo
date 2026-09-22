# 런처 모델 자동 갱신 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 런처 재열기에 모델 목록을 안전하게 갱신하고 Cursor·Claude의 실제 CLI 가용 모델을 연결한다.

**Architecture:** 파일 감지는 기존 LazyBoundedWorker를 사용한다. 제공자별 성공/실패와 계정 범위를 App snapshot에서 병합하고 UI 조작이 끝난 뒤 게시한다. 외부 CLI 조회는 별도 bounded worker로 분리한다.

**Tech Stack:** Rust, serde/serde_json/toml, egui, 기존 LazyBoundedWorker, 설치된 CLI.

---

## 상태와 PR 경계

기준 작업본은 `deppy-sijo-agent-wait-audit`, HEAD `881e94d`다. 기존 sidebar/A1 등의 dirty
변경은 별개이며 이번 변경과 통째로 커밋하지 않는다. 아래 ID는 GitHub PR 번호가 아니다.
사용자는 설계 보완과 개발 착수를 승인했다. 추가 승인 질문 없이 MC1부터 진행한다.

| 단위 | 변경 책임 | 의존성 | 완료 기준 |
| --- | --- | --- | --- |
| PR-MC1 | 파일 결과 구분·읽기 상한·정상 빈 목록 | 없음 | 집중 파서/런처 테스트 |
| PR-MC2 | 보완 3~6: 안전 병합·선택·모든 open·UI 안정성 | MC1 | 로직 테스트 및 요청 후 화면 확인 |
| PR-MC3 | 보완 1: Cursor 목록/선택/실행 | MC2 | 실제 목록 fixture·실행 인자·timeout 회수 |
| PR-MC4 | 보완 2: Claude SDK 지원 모델 | MC2, MC3의 probe 기반 | 설치 CLI 초기화 검증·지원/미지원 폴백 |

MC1은 자동 재감지를 아직 켜지 않는 독립 변경이다. MC2가 계정 범위 보존을 갖춘 뒤
자동 갱신을 켠다. MC3/MC4는 파일 감지 worker의 지연을 늘리지 않는다.

## PR-MC1: 읽기 결과와 메모리 상한

**Files:**
- Modify/Test: `crates/app/src/agent_model_catalog.rs` — 읽기 오류 분류, 기존 파서 공유, 입력 상한.
- Modify/Test: `crates/app/src/agent_launcher.rs` — Ready 빈 목록과 내장 폴백 구분.
- Update: `docs/CODEX_HANDOFF.md` — 실제 명령/결과/남은 작업.

- [x] RED: 실제 임시 홈의 정상 빈 목록에 내장 모델이 되살아나는 회귀를 추가한다.

```rust
// 빈 모델 배열을 가진 캐시를 임시 홈에 쓴 후:
assert!(resolve_models(AgentKind::Codex, Some(home), None).is_empty());
assert!(resolve_models(AgentKind::Grok, Some(home), None).is_empty());
```

- [x] 아래 집중 명령으로 RED를 확인한다. 기존은 builtin 목록이 나와 실패해야 한다.

```sh
CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/private/tmp/deppy-ready-prs-integration-target cargo test --locked -p deppy-sijo --bin deppy-sijo --features bench-alloc catalog_refresh -- --nocapture
```

- [x] 읽기 결과를 정의하고 같은 역직렬화 결과에서 모델을 만든다. 검증용으로 전체 JSON/TOML을 두 번 파싱하지 않는다.

```rust
pub(crate) enum CatalogLoad {
    Ready(Vec<ModelChoice>),
    Missing,
    Unavailable,
    Unsupported,
}
```

- [x] 파일 읽기는 `File`의 `Read::take(CATALOG_MAX_BYTES + 1)` 뒤 UTF-8/길이를 검증한다. Missing은 NotFound만, 그 외 읽기/스키마 실패는 Unavailable이다. Codex의 models 필드는 필수로 바꾼다.
- [x] `resolve_models`는 `Ready(models)`를 그대로 사용하고 다른 상태만 builtin으로 폴백한다. 이미 비어 있는 목록에는 configured 모델을 억지로 삽입하지 않는 기존 adopt_model 계약을 유지한다.
- [x] 실제 임시 파일로 Missing→손상→정상 빈 목록→정상 항목을 확인한다. 각 제공자의 비정상 루트 스키마를 Unavailable로 검증한다. 8MiB 초과/비UTF-8 입력을 거절한다.
- [x] `agent_model_catalog::tests`와 `agent_launcher::tests` 필터로 해당 로직만 GREEN 확인한다. 코드 자체로 결과를 모사하는 테스트를 추가하지 않는다.
- [x] 변경 diff와 plan/spec 계약을 직접 리뷰하고 인계에 결과를 남긴다. 커밋 시에는 위 두 Rust 파일과 이번 문서 hunk만 선택한다.

## PR-MC2: 자동 갱신을 안전하게 연결

**Files:** `crates/app/src/agent_launcher.rs` (snapshot 병합/범위), `crates/app/src/agent_model_catalog.rs` (범위 증거), `crates/app/src/ui/agent_launcher.rs` (선택/열기/표시), `crates/app/src/app.rs` (결과 게시).

- [x] 먼저 제공자별 비밀 아닌 계정 범위 소스를 코드/실제 CLI와 대조한다. Grok 캐시의 identity/origin/auth_method는 정상 캐시에서만 읽고, 손상 때 비교 가능한 독립 범위가 없으면 보존을 끈다. Codex auth/config 변경 메타데이터, Kimi/Qwen 설정 변경을 범위 무효화로 사용한다. 비밀 원문은 snapshot에 담지 않는다.
- [x] 순수 병합 테스트로 같은 범위의 일시 실패만 이전 모델을 유지하고, Missing/Ready([])/계정 변경/설치 교체/10분 만료에는 이전 모델을 제거함을 검증했다. 실패가 성공 시각을 연장하지 않는 연속 실패 사례도 확인했다.
- [x] snapshot별 provider 상태·성공 시각을 한 벌만 보관하고 UI 스레드 파일 I/O 없이 병합한다. 실패에서 `default_model/default_effort`를 성공처럼 덮지 않는다. 기존 직접 Claude 기본값 갱신의 ignore-next-completion 계약도 보존한다.
- [x] 모델·강도에 독립 explicit 플래그를 두고 순수 선택 테스트를 작성한다: 자동 4.6→새 설정 4.7, 명시 선택 4.6 보존, 강도만 명시 선택, 모델 소멸, 제공자 전환. 유효한 명시 선택 외에는 새 agent의 initial_model/initial_effort를 따른다.
- [x] `open_for`의 소비 가능한 갱신 요청을 세 경로에서 확인한다. `open_for_kind`가 열기에 실패하면 요청을 만들지 않는다. App은 요청을 기존 detection_requested에 합쳐 단일 in-flight + 후속 요청 1개만 유지한다.
- [x] popup/pointer 조작 및 launch_pending에는 결과 1개를 보류한다. App의 최신 적용 snapshot과 UI 실행 검증이 서로 다른 목록을 사용하지 않게 같은 게시 시점에 교체한다.
- [x] spinner를 기존 24pt 헤더 안으로 옮기고 모델 항목을 provider/model ID로 고정한다. 조용한 갱신은 리스트 높이·스크롤을 재설정하지 않는다.
- [x] 병합/선택 집중 테스트 실행 완료. 실제 앱의 cold/warm open·빠른 재열기·드롭다운 열린 중 갱신·NewRun 화면 검증은 아래 별도 미완료 항목으로 관리한다.

## PR-MC3: Cursor 모델 목록과 실행

**Files:** Create `crates/app/src/agent_model_probe.rs` (제한된 CLI 수명/목록 파서), Modify `crates/app/src/main.rs` (모듈 등록), `agent_launcher.rs`, `app.rs`, `ui/agent_launcher.rs`.

- [x] 설치된 `cursor-agent --list-models`의 실제 출력을 10초/256KiB 한도에서 확인한다. 모델 ID/라벨만 비밀 제거 fixture에 남기며 출력 스키마를 추측하지 않는다. JSON이 없는 버전은 ANSI 제거 후 확인된 목록 행만 허용한다.
- [x] fixture에 신규 ID·중복·긴 문자열·control 문자·오류 안내 행을 넣어 파서를 먼저 검증한다. 성공 exit와 유효 목록 형식이 둘 다 확인돼야 Ready다. 빈 stdout은 정상 빈 목록으로 간주하지 않는다.
- [x] `AgentKind::Cursor.supports_model` 및 `build_launch_spec`을 함께 연결하고 `--model <검증된 ID>` 실행 인자를 검증한다. CLI 미지원 강도를 합성하지 않는다.
- [x] 독립 worker에서 조회하고 MC2 게시/범위 계약을 재사용한다. 인증 실패는 이전 계정 목록을 지운다. 범위 증거 없이 timeout 결과에 오래된 모델을 보존하지 않는다.
- [x] fake 실행 파일로 stdout 폭주·stderr 폭주·timeout·자식 프로세스가 파이프를 계속 잡는 경우 종료/회수를 확인한다. 실제 CLI는 목록 조회만 하며 LLM 요청을 보내지 않는다.

## PR-MC4: Claude 지원 모델과 별칭

**Files:** `agent_model_probe.rs`, `agent_launcher.rs`, `agent_model_catalog.rs`, `app.rs`, `ui/agent_launcher.rs`, `crates/i18n/locales/{en-US,ko-KR,ja-JP,zh-Hans,zh-Hant}/messages.txt`.

- [x] 공식 SDK의 현재 initialize 요청/모델 응답을 설치 CLI의 지원 옵션과 대조한다. hooks/MCP/프로젝트 설정/저장을 막을 수 있는 버전만 격리 디렉터리에서 프롬프트 없이 초기화한다. 응답이 없으면 Unavailable로 종료한다.
- [x] 실제 응답의 모델 value/displayName 및 명시된 effort capability를 fixture로 만든다. unknown capability는 빈 강도 목록으로 두고 기존 CLAUDE_EFFORTS를 모든 신규 모델에 복사하지 않는다.
- [x] 지원 응답 파서, request ID 불일치·잘못된 응답·지원되지 않는 CLI·timeout 테스트를 추가한다. 계정/API/provider 차이는 MC2의 범위 변경으로 취급한다.
- [x] 동적 응답 성공 시 그 목록과 유효한 configured 모델을 연결한다. 미지원/최초 실패에는 기존 별칭을 유지하며, 별칭 해석을 앱에서 버전 ID로 고정하지 않는다.
- [x] Cursor와 같은 제한된 worker/게시 경로를 사용한다. 실제 CLI 모델 발견과 별칭 폴백을 구분하여 결과를 인계한다. 검증하지 못한 CLI에서 동적 발견 완료로 기록하지 않는다.

## 검증과 진행 기록

- 공통 Rust 명령은 MC1의 동일 환경/패키지에서 테스트 필터만 바꾼다. 전체 테스트 반복과 release 빌드는 이번 작업 범위 밖이다.
- 커밋 전 게이트는 한 번만 수행하며 기존 dirty 변경을 섞지 않는다. GitHub PR 생성·커밋·merge 상태를 코드 구현 상태와 구분해 보고한다.
- [x] 원 설계 여섯 보완 계약 반영, PR 의존성/파일 책임/실패 정책 수립.
- [x] MC1 구현/집중 검증 완료 — catalog 36건 + launcher 필터 52건 통과(2026-09-22), 미커밋.
- [x] MC2 구현/집중 검증 완료 — 화면 확인은 별도 대기.
- [x] MC3 구현/실제 CLI 계약 검증 완료 — macOS Cursor 64개(표시 상한) 확인.
- [x] MC4 구현/실제 CLI 계약 검증 완료 — macOS Claude 5개 확인.
- [ ] 사용자 요청 후 release 빌드·화면 검증.

### MC1 실제 검증 결과 (2026-09-22)

- 빈 카탈로그 회귀 RED: exit101, Codex 내장 7개가 재등장. 수정 후 신규 집중 4건 통과.
- 추가 루트 스키마 RED: `[[]]`를 `Ready([])`로 잘못 수용. JSON 객체만 허용하도록 수정.
- 최종 `agent_model_catalog::tests`: 36 passed, 0 failed. 로그 `/tmp/deppy-catalog-mc1-catalog-final.log`.
- 최종 `agent_launcher::tests`: 52 passed, 0 failed. 이 필터는 기존 ui::agent_launcher 테스트도 포함한다. 로그 `/tmp/deppy-catalog-mc1-launcher-final.log`.
- 두 파일 rustfmt 적용 완료. 새 의존성/상시 스레드/매 프레임 I/O/중복 JSON 파싱을 추가하지 않았다. Kimi 카탈로그 내 기본값 재파싱도 제거했다.
- MC2~4는 미구현이며 자동 재감지도 아직 활성화하지 않았다. 실제 화면·release 빌드·재실행·커밋·GitHub PR 생성은 수행하지 않았다.

### MC2~4 실제 구현 결과 (2026-09-22)

- 파일 감지는 기존 worker, Cursor/Claude는 별도 lazy worker로 연결했다. 세대 ID와 실행 파일 metadata revision을 비교하여 늦은 결과를 폐기한다. UI 조작/세션 시작 중에는 최신 snapshot 1개만 보류한다.
- 동일 설치/계정 범위의 디스크 읽기 실패만 10분 이내 보존한다. Grok auth.json의 저장/로그아웃 계약은 설치 CLI의 배포 문서로 확인했다. 비밀 내용은 읽지 않았다. 외부 CLI 인증 범위를 증명하지 못한 실패에는 기존 폴백을 사용한다.
- 설치 Claude SDK 응답은 지원 강도만 선언하고 기본 강도는 제공하지 않는다. 사용자 강도 설정도 없을 때 임의 Low로 낮추지 않도록 CLI 기본값(None, --effort 생략)을 추가했고 5개 로케일에 반영했다. 사용자 명시 선택을 보존한다.
- 프로세스 제한: 10초(Claude help 포함), stdout/stderr 각각 256KiB, 제공자별 64개 모델. 비차단 파이프 + 소유 process group 종료/회수로 출력 폭주·timeout·부모 종료 뒤 파이프를 잡은 자식을 처리한다. Unix 구현이며 Windows 외부 조회는 기존 폴백이다.
- 실제 CLI 조회: Cursor 64개(상한 적용), Claude 5개. 로그: `/tmp/deppy-mc3-installed-cursor.log`, `/tmp/deppy-mc4-installed-claude.log`. 프롬프트/LLM 생성 요청 없이 목록 조회 또는 initialize만 수행했다.
- 마지막 검토에서 감지 실패가 진행 중 세션 시작 상태를 취소하는 경로를 RED로 확인하고 수정했다. CLI 기본 강도 미확인을 Low로 바꾸는 경우도 RED→GREEN으로 보완했다.
- 최종 집중 검증: `catalog_refresh` 11 passed (`/tmp/deppy-mc-final-refresh.log`); `model_probe_` 4 passed, 실제 CLI용 1 ignored (`/tmp/deppy-mc-final-probe.log`); 실제 CLI ignored 테스트는 위처럼 각각 별도 실행해 통과했다. 기존 launcher 순수 로직 34 passed (`/tmp/deppy-mc-final-launcher.log`), Grok 기존 선택 로직 1 passed (`/tmp/deppy-mc-final-grok-selection.log`).
- 실제 앱 UI 확인, release 빌드·재실행, GitHub PR 생성·merge는 별도 후속 작업이다. PR 단위 ID를 실제 GitHub 번호로 표시하지 않는다. 커밋 전 최종 리뷰는 아래에 기록한다.

### 커밋 전 리뷰와 최종 검증 (2026-09-22)

- 설치 감지에도 요청 세대를 연결했다. 이전 감지 결과가 새 열기의 모델 조회 요청을 먼저 소비하는 결함을 RED→GREEN으로 수정했다. 빠른 재열기 두 번과 이전 결과의 지연 도착을 확인한다.
- 외부 조회 중에는 고정 헤더의 로딩 표시를 유지하고, worker 자체 실패도 일반 조회 실패와 동일하게 정리한다. 제공자별 실행 중 요청은 각각 한 건만 보관한다.
- 기존 sidebar/A1/Fleet/검색/파일 트리 작업을 제외한 커밋 대상 소스만 임시 복사본으로 내보내 검사했다. fmt, workspace/all-targets clippy(`-D warnings`, `deppy-sijo/bench-alloc`), i18n-check, staged diff 검사 모두 통과했다.
- 해당 소스의 모델 카탈로그·런처 순수 로직·probe 집중 검사 **76 passed / 0 failed / 1 ignored**. 앱 UI suite는 실행하지 않았다. 앞서 갱신 상태 집중 검사는 12 passed였다. 필터 간 중복을 고유 테스트 수로 합산하지 않는다.
- 정확한 명령/증거 경로/커밋 범위는 `docs/CODEX_HANDOFF.md`의 최상단 리뷰 섹션을 따른다. release 재빌드·재실행·push·merge는 수행하지 않는다.
