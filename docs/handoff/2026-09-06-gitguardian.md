# PR #146 GitGuardian Security Checks 실패 진단 (읽기 전용)

- 조사 일시: 2026-09-06
- 저장소: JRVector9/deppy-sijo, PR #146 (`feat/fleet-one-list-and-relay-wip` → `main`)
- 조사 범위: 읽기 전용. 커밋/푸시/억제 설정/키 삭제/시크릿 회전 없음. 앱 빌드·실행 없음.
- 원칙: 실제 토큰/키 값은 이 문서에 기록하지 않는다. 파일 위치·유형·기존 여부만 기록한다.

## 1. 확인된 사실 (근거 포함)

### 1.1 체크런 결과 (gh api repos/:owner/:repo/commits/<sha>/check-runs)

현재 커밋 `3db4952`:

| 검사 | 결론 |
|---|---|
| Build, clippy, and test (macOS) | success |
| Format, boundary, and diff checks | success |
| Relay protocol and server (Linux) | success |
| Relay WebCrypto concurrency | success |
| RustSec audit | success |
| Licenses, sources, and duplicate versions | success |
| **GitGuardian Security Checks** | **failure** |

- GitGuardian output title: `1 secret uncovered!`
- GitGuardian output summary: `#### 1 secret were uncovered from the scan of 3 commits in your pull request. ❌`
- `details_url`: `https://dashboard.gitguardian.com` (커밋/파일/라인 정보 없음)
- check-run id: `101395371666`

### 1.2 부모 커밋 및 PR 첫 커밋 비교 — 각각 1건 보고

| 커밋 | 스캔 대상 | GitGuardian |
|---|---|---|
| `53f2a31` (PR 1번째) | 1 commit | failure — "1 secret was uncovered from the scan of 1 commit" |
| `e960004` (부모) | 2 commits | failure — "1 secret were uncovered from the scan of 2 commits" |
| `3db4952` (현재) | 3 commits | failure — "1 secret were uncovered from the scan of 3 commits" |

→ 스캔 커밋 수가 1→2→3으로 늘어도 보고된 발견 수는 각각 1건이다.
→ 실패는 PR의 첫 커밋 `53f2a31`부터 있었다. 개별 incident 식별자가 공개되지 않아 같은 발견인지까지 확정하지 않는다.
→ 참고: 부모 `e960004`에서는 `Build, clippy, and test (macOS)`도 failure였으나 현재 커밋에서는 success로 회복됨.

### 1.3 주석(annotations) 없음

- `gh api repos/:owner/:repo/check-runs/101395371666/annotations` → `[]` (빈 배열)
- 즉 **GitHub API 경로로는 어떤 파일/라인이 걸렸는지 알 수 없다.** GitGuardian GitHub App은 상세를 대시보드에만 남긴다.

### 1.4 저장소에 GitGuardian 설정 파일 없음

- `.gitguardian.yaml` / `.gitguardian.yml` / `.ggshield*` 없음
- 저장소 전체 `rg -i 'gitguardian|ggshield'` → 히트 없음
- → 워크플로 로그로 재현 불가. 검사는 GitHub App이 외부에서 실행한다.

## 2. 원인 후보 (정황 기반, 확정 아님)

PR diff(91파일, +20661)에서 시크릿 탐지기가 반응할 만한 항목을 커밋 `53f2a31` 기준으로 좁혔다.

### 2.1 조사 후보 — 신규 테스트 픽스처

- 파일: `crates/web-remote/tests/fixtures/relay-hello-v1.json` (`53f2a31`에서 **신규 추가 A**, 96줄)
- 유형: 64자리 hex 값 8건. 비밀값을 의미하는 다음 키 이름이 있지만 실제 탐지기는 미확인이다:
  - `desktop.ephemeral_private_scalar_hex`
  - `desktop.identity_private_scalar_hex`
  - `device.ephemeral_private_scalar_hex`
  - `device.identity_private_scalar_hex`
  - `pairing_proof.secret_hex`
  - 그 외 `connection_id_hex`, `identity_fingerprint_hex`, `proof_hex` 등
- **값 자체는 이 문서에 옮기지 않았다.**

### 2.2 차순위 후보 — 인계 문서의 SHA-256

- 파일: `docs/CODEX_HANDOFF.md` (1MB 이상 — 전체 읽지 않고 `rg` + 앞부분만 확인)
- `53f2a31`에서 새로 추가된 64자리 hex 포함 줄은 **2줄**뿐이며, 문맥상 모두 macOS 배포 산출물의 SHA-256 체크섬(`Sijo.app` / `Sijo.zip` 무결성 해시)이다. 시크릿이 아니다.
- 파일 전체 기준 hex64 매치 68건 대부분도 같은 성격(기존 줄 포함).

### 2.3 배제된 후보

- `deploy/relay-shell/{production,staging}/shell.env.example`: 값이 전부 자리표시자(`https://<host>` 형식). 파일 자체에 "실제 자격증명을 커밋하지 않는다" 명시. 실제 비밀값 없음.
- `Cargo.lock`: hex64 매치 617건이지만 전부 crate 체크섬. 표준 탐지기가 무시하는 범주.
- AWS/GitHub/OpenAI 토큰 접두 패턴(`AKIA`, `ghp_`, `sk-`), PEM `BEGIN ... PRIVATE KEY`, JWT(`eyJ...`): PR diff 전체에서 **매치 0건**.

## 3. 해당 픽스처가 테스트 데이터라는 근거

`relay-hello-v1.json`이 원인이라는 전제 하에, 다음 근거로 **테스트용 고정 벡터이며 실제 자격증명이 아니라고 판단**한다.

1. 파일 내 `note` 필드가 생성 근거를 명시한다(원문 요지):
   - "Relay pre-E2EE hello record layouts (v1). Rust 구현과 독립적으로 작성된 인코더로 생성했으므로 이 픽스처는 코드가 아니라 명세를 고정한다."
   - "Every scalar here is a fixed test value and **no production key material appears in this file**."
2. 위치가 `tests/fixtures/` 하위이며, `crates/web-remote/tests/relay_shell_chrome.rs` 등 테스트에서 소비된다. 런타임 코드 경로가 아니다.
3. 동일 성격의 자료가 **이미 `main`에 존재한다**: `crates/web-remote/tests/fixtures/relay-webcrypto-v1.json`(`ephemeral_private_jwk`, `identity_private_jwk`, `shared_secret_hex` 포함), `relay-webcrypto-v1.js`, `crates/relay-protocol/tests/fixtures/relay-wire-v1.json`. 즉 신규 유형의 자료가 아니라 **기존 관행의 연장**이다.

→ 이 픽스처는 테스트용이라고 문서화되어 있다. **GitGuardian이 지목한 대상이 이 파일인지 확인하지 못했으므로 실제 경고의 오탐 여부도 미확정이다.**

## 4. 확인 불가 범위 (한계로 남김)

1. **GitGuardian 대시보드 접근 권한 없음.** `details_url`이 `https://dashboard.gitguardian.com`이며, 이 세션에는 GitGuardian 계정 자격이 없다. 따라서 탐지기 이름, 지목된 파일·라인, incident 상태(신규/기존/무시됨)를 **직접 확인할 수 없다.**
2. **check-run annotations가 비어 있어** GitHub API만으로는 대상 특정이 불가능하다.
3. **저장소에 ggshield 설정/워크플로가 없어** 로컬 재현으로 대조할 수단이 없다(재현 실행도 이번 범위 밖).
4. **브랜치 보호 규칙 확인 불가**: `gh api .../branches/main/protection` → HTTP 403 "Upgrade to GitHub Pro or make this repository public". 따라서 GitGuardian 검사가 **머지 필수 검사인지 여부를 판정하지 못했다.**
5. `main`에 이미 있는 유사 픽스처(`relay-webcrypto-v1.json`)를 추가한 커밋 `e3617ea`에는 GitGuardian 체크런 기록이 **없다**. App 설치 시점 이후부터만 스캔된 것으로 보이나, 설치 시점은 확인하지 않았다. → 과거에 통과했거나 스캔되지 않았다고 단정할 수 없고, 확인 가능한 체크런 기록이 없다는 뜻이다.

## 5. 수행하지 않은 것 (지시대로 금지 준수)

- 억제/무시 설정 추가 없음 (`.gitguardian.yaml`, `# ggignore` 등 일절 생성 안 함)
- 키·픽스처 삭제 없음
- 시크릿 회전 없음
- 코드 커밋·푸시 없음
- 앱 빌드·실행·종료 없음
- 추가 서브에이전트 스폰 없음
- 실제 시크릿 값 출력·복사 없음 (본 문서 및 터미널 출력 모두 마스킹 처리)

## 6. 다음 사람이 해야 할 판단 (실행 아님, 제안만)

1. GitGuardian 대시보드에서 incident를 열어 **탐지기 이름과 지목 파일**을 확인한다. 2.1 후보와 일치하는지 대조하면 진단이 확정된다.
2. 지목 대상과 테스트 전용 사용을 확인한 뒤에만 오탐 처리 또는 수정 범위를 결정한다. 현재 정보만으로 경고를 억제하지 않는다.
3. 만약 지목 대상이 2.1이 아니라면 이 문서의 배제 목록(2.3)부터 재검토한다.
