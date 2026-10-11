| Priority | Location | Finding | Impact | Next step |
| --- | --- | --- | --- | --- |
| high | `crates/app/src/ui/env_profiles.rs:1162`, `crates/app/src/app.rs:3970` | dotenv 키는 API 목록에서 빠지고 환경 행에 수정 진입 없음 | 이미 저장한 프로젝트 키를 쉽게 교체할 수 없음 | PR 1: 출처별 키 통합 목록과 수정 버튼 |
| high | `crates/app/src/app.rs:34357`, `crates/runtime/src/in_process.rs:1438` | 파일 보관과 수동 API 우선 연결이 분리됨 | 파일 갱신과 실제 실행값이 다를 수 있음 | PR 2: 키별 원본·우선순위·사용처 명시 |
| medium | `crates/app/src/ui/credentials.rs:1181`, `crates/app/src/ui/env_profiles.rs:1054` | 값은 painter 텍스트이며 복사 동작 없음 | 표시해도 더블클릭·버튼으로 키 값 복사 불가 | PR 1: 값 영역 더블클릭과 복사 버튼 |
| medium | `crates/app/src/app.rs:3658`, `crates/runtime/src/in_process.rs:3018` | 실행 준비는 있지만 빌드별 최신값 확인·사용 버전 기록 없음 | 오래된 프로세스에서 어떤 값을 쓰는지 확인 어려움 | PR 3: 빌드 wrapper와 사용 기록 |
| medium | `crates/storage/src/workspace_env.rs` | 프로젝트 API 연결은 로컬 환경변수 연결이며 Actions Secret 목적지 아님 | 저장한 키의 GitHub 반영 여부를 관리할 수 없음 | PR 4: 명시적 GitHub 목적지·전송 상태 |
| medium | `crates/app/src/dotenv_sync.rs:759` | 환경파일 수정은 여러 줄 값을 거부하며 PEM 파일 원본 연결 없음 | GitHub App 개인키도 문자열 키처럼 관리하기 어려움 | PR 2: 개인키 파일 보관·교체 감지 |

# 프로젝트 키 관리 재검토 · 2026-10-12

요청: nomorevibe 등 프로젝트에서 쓰는 API 키·GitHub 관련 비밀값을 쉽게 보관하고, 원본 변경 후 보관값도 갱신하며, 빌드에 최신 값을 쓰게 만드는 수정 사항과 UI 개선안. 추가 요청: 이미 입력한 키 수정과 키 값 더블클릭 복사.

**이 문서는 조사와 구현 제안이다. 제품 코드는 수정하지 않았다. 실제 키 값은 조회하지 않았으며 Keychain·환경파일·GitHub·클립보드를 변경하지 않았다. 앱 재시작도 하지 않았다.**

- 검토 소스: `/Users/jr/Desktop/projects/deppy-sijo-performance`, HEAD `29c16836`, 버전 `0.10.3`.
- 현재 실행 앱: PID `21173`, `target/restart-0.10.1-20261011/Deppy Sijo.app/Contents/MacOS/deppy-sijo` 경로. 소스와 실행 앱 버전을 구분한다.
- 시안: [프로젝트 키 관리 HTML](../mockups/project-keys-proposal-2026-10-12.html).
- 기존 API 사용 안내: [2026-10-10 검토](../reviews/2026-10-10-api-settings-review.html). 이번 요청은 단순 키 보관을 넘어 프로젝트 원본·빌드·GitHub 반영 흐름까지 포함한다.

## 실제 nomorevibe 저장 상태

실행 프로세스가 연 DB 경로를 `lsof -p 21173 -Fn`으로 확인한 뒤 SQLite `mode=ro`로 메타데이터만 읽었다. `keyring_username`, `masked_hint`, `oauth_json`, `plain_value`는 SELECT 하지 않았다. `.env` 값과 OS Keychain을 읽지 않았다.

| 확인 항목 | 관찰 |
| --- | --- |
| 프로젝트 | `/Users/jr/Desktop/projects/nomorevibe` |
| 환경 프로필 | `.env`, kind=`dotenv`, 운영 구분 0 |
| 환경 변수 | 14개, 그중 Keychain 참조 secret 8개 |
| 비밀값 이름 | `AUTH_SECRET`, `ADMIN_TOKEN`, `CRON_SECRET`, `DATABASE_URL`, `VISITOR_HASH_SECRET`, `GITHUB_TOKEN`, `GITHUB_STAR_SECRET`, `GITHUB_FEEDBACK_SECRET` |
| 수동 API 환경 연결 | `workspace_credential_env` 0개 |
| 파일 설정 | 별도 override 없음 → 기본 `.env`, `.env.local` |
| 최근 저장 시각 | `GITHUB_STAR_SECRET`, `GITHUB_FEEDBACK_SECRET`의 메타데이터 갱신이 2026-10-11 UTC에 존재 |

따라서 “프로젝트 값 보관과 자동 갱신이 전혀 없다”는 설명은 틀리다. 파일 기반 보관 기능은 있고 최근 갱신도 관찰된다. 다만 최근 갱신 시각만으로 실제 키 내용·GitHub 원격값·빌드 사용값의 일치까지 검증했다고 말할 수 없다.

현재 UI 자동화를 통해 기존 키의 실제 변경·복사를 실행하지 않았다. 스크린샷/접근성 관찰과 소스·DB 메타데이터를 구분하며, 제품 UI 실동작 PASS를 주장하지 않는다. 시안의 파일별 출처, 버전, 빌드/GitHub 상태는 예시이며 실제 DB 상태를 시뮬레이션한 것은 아니다.

## 근본적인 사용 흐름 문제

### 1. 저장한 키의 종류에 따라 수정 진입이 달라진다

- `app.rs:3970` 부근 `load_environment_snapshots`는 dotenv에서 참조하는 credential을 API 목록에서 제외한다.
- `ui/env_profiles.rs:994`, `:1162`의 `EnvRowResponse`는 `delete`와 `toggle_reveal`만 반환한다. 환경 목록 행에 직접 수정·복사 intent가 없다.
- 수동 API는 `ui/credentials.rs:1259`의 `meta.editable && response.double_clicked()`로 편집한다. 동일 프로젝트가 소유한 `api_key`/`token`만 editable이다(`app.rs:3974`). 명시적 수정 버튼이 없다.
- `ui/credentials/edit.rs:35`, `:87`의 편집 폼·저장 경로는 존재한다. 새 값을 비우면 기존 값을 유지하고, 새 값을 주면 논리 ID를 유지한 교체가 가능하다. 전체 저장 backend가 없어서 수정이 불가능한 것으로 단정하면 안 된다.
- 환경파일 쓰기 backend도 있다: `SettingsJobAction::WriteDotenv` → `write_env_var_in_sources`(`app.rs:4905`, `dotenv_sync.rs:721`). 이를 환경 목록의 **수정** 버튼에서 사용할 수 있게 연결해야 한다.

**수정 방향:** 프로젝트와 환경을 고정한 통합 목록에서 `.env` 원본 키와 보관함 원본 키를 함께 보여준다. 키 이름은 상세 보기, 값 영역 더블클릭은 복사, 수정은 명시적 버튼으로 분리한다. dotenv 원본은 해당 실효 파일을 수정하고 보관값을 갱신한다. 보관함 원본은 Keychain 교체와 기존 참조 유지 경로를 사용한다. 원본 파일이 둘 이상이면 실효 원본과 변경 대상 파일을 표시한다.

### 2. 키 값을 표시하는 것과 복사하는 것은 별도 기능이다

- API 값은 `credential_table_row`의 `painter.text`로 그린다. 환경 값도 `env_table_row`에서 그린다. 일반 선택 가능한 입력 컴포넌트가 아니다.
- 두 행 구현에 `copy_text`/클립보드 동작이 없다. 현재 행 더블클릭은 API 수정이며 값 복사가 아니다.

**수정 방향:** 별도 `Copy` intent를 worker에 전달하고 현재 프로젝트·키 ID·revision이 유효할 때만 비밀값을 임시로 읽어 로컬 클립보드에 전달한다. 기본 숨김은 유지한다. 잠금·키 삭제·읽기 실패는 행/상세 안에 원인을 표시한다. 키보드로 접근 가능한 복사 버튼도 제공한다. 원격만 존재하는 GitHub Secret은 로컬 값 미보관임을 표시하고 복사를 비활성화한다.

임시 UI 값/undo/로그/에러에 비밀을 남기지 않는다. 선택 가능한 30초 후 클립보드 정리는 **앱이 넣은 값이 아직 남아 있는 경우에만** 수행하여 그 뒤 사용자가 복사한 다른 내용을 지우지 않는다. HTML에서는 실제 클립보드를 변경하지 않고 성공 상태만 시연한다.

### 3. 자동 보관의 원본과 실행 우선순위가 잘 보이지 않는다

- `dotenv_sync.rs:1280` 이후 secret 분류·Keychain 저장·교체 기능이 있다. 같은 secret 값이면 불필요한 교체를 생략한다.
- `core/env_sources.rs:2` 기본 파일은 `.env`, `.env.local`이다. 선택 가능한 root 파일명 목록을 별도 저장할 수 있다(`storage/workspace_env.rs:25`). 현재 자동으로 빌드 mode를 추론해 파일 목록을 고르는 기능은 아니다.
- `app.rs:34357`에서 활성 파일 트리 watcher의 선택 경로 변화가 `SyncDotenv`를 올린다. 실행 준비 worker와 수동 재동기화 경로도 있다. 파일 트리와 무관하게 모든 등록 프로젝트의 키 변경을 지속 관리하는 독립 서비스로 보기는 어렵다.
- `app.rs:3980`은 수동 API binding의 dotenv 덮어쓰기 상태를 만들고, runtime은 dotenv secret 뒤 API secret을 추가한다. 같은 이름을 따로 등록하면 파일의 새 값보다 수동 등록값이 실제 실행에 우선할 수 있다.
- `env_reload.rs:1`의 live reload는 기존 zsh의 일반 단일 행 값을 다음 프롬프트에서 갱신하는 옵트인 기능이다. 비밀값이나 실행 중 agent 전체를 최신 값으로 바꾸는 기능이 아니다.

**수정 방향:** 키마다 `workspace identity + 환경 + 이름 + 원본 + 사용 대상`을 관리한다. 기존 연결을 무조건 합치거나 뜻을 바꾸지 않는다. 중복은 실효값과 우선 원본을 확인하도록 안내한다. 연결한 파일만 bounded worker에서 감시하고 파일 교체·이벤트 폭주·잠자기 복귀·inactive 프로젝트·앱 재시작을 재검사한다. 읽기/보관 실패에서 정상 보관본을 유지하며 실패한 변경은 재시도한다. 삭제/빈 값은 값 교체와 다른 상태로 기록해 조용히 사용 가능 키를 없애지 않는다.

### 4. 빌드와 기존 세션은 다르게 취급해야 한다

`execute_dotenv_sync_job`과 `RuntimeCommand::SetSessionDefaultEnv`는 새 shell/agent 실행에 값을 전달한다. 이미 실행한 프로세스의 OS 환경은 이를 통해 바뀌지 않는다. 프로젝트의 build command마다 최신 원본을 다시 검사하고 사용 버전을 기록하는 계층은 확인되지 않았다.

**수정 방향:** 프로젝트별 빌드 명령을 등록하거나 `deppy env run -- <command>` 같은 wrapper를 **새로 설계**한다(현재 제공 명령으로 소개하지 않는다). shell에서 wrapper를 쓰거나 앱/MCP의 연결된 빌드 실행기를 통해 최신 snapshot을 자식 프로세스에 주입한다. 일반 외부 터미널에서 임의로 실행한 모든 명령을 자동 보장할 수 있다고 설명하면 안 된다.

빌드 직전 원본 재확인 → 필수값·충돌 검사 → 고정 snapshot 확정 → 빌드 프로세스 실행 → 이름/버전/시각만 기록한다. 실행 중인 빌드의 값은 이후 rotation으로 바꾸지 않는다. 빌드 자체가 새 키를 발급하지 않는다. 빌드 도구/에이전트가 원본 파일을 실제로 변경하면 파일 변경 경로에서 저장한다. 프로젝트 프로그램용 키와 agent 인증 전달 범위는 별도로 설계하고 기존 binding 의미는 보존한다.

### 5. GitHub 인증 토큰과 Actions Secret의 목적지가 다르다

`GITHUB_TOKEN`을 로컬 Keychain/환경에 보관하는 것과 `owner/repo`의 Repository·Environment·Organization Secret을 업데이트하는 것은 별개의 기능이다. `workspace_credential_env`는 로컬 변수명 연결이며 remote destination 필드가 없다. `crates/**/*.rs`의 `gh secret`, `actions/secrets`, `repository_secret`, `github_secret` 검색에서 Actions Secret 동기화 구현을 발견하지 못했다.

[GitHub 공식 REST 문서](https://docs.github.com/en/rest/actions/secrets#get-a-repository-secret)는 Secret 조회에 값이 포함되지 않는다고 명시한다. 생성/수정에는 공개키로 암호화한 값과 해당 범위의 쓰기 권한이 필요하다. 로컬 보관값의 전송은 구현할 수 있지만 원격에서 바뀐 실제 값은 조회해서 보관할 수 없다.

**수정 방향:** 인증 계정/권한, 고정 repo identity, 정확한 secret scope/environment/name, 자동 전송 여부를 연결 시 선택한다. 저장 성공과 원격 전송 성공을 분리하고, 보관 revision·마지막 전송 revision·remote 메타데이터/시각을 기록한다. 원격에서만 교체되면 `외부 변경 · 값 미확인`을 표시하고 자동 덮어쓰기를 보류한다. 새 값은 사용자 입력 또는 연결된 읽기 가능한 원본이 필요하다.

성공 상태는 `전송 완료`로 표시한다. `양쪽 값 동일` 검증으로 과장하지 않는다. 메타데이터 preflight만으로 원격 compare-and-swap이나 동시에 일어난 외부 쓰기의 완전 차단을 보장하지 않는다. 네트워크/권한 실패는 로컬 보관본을 유지하고 durable outbox로 재시도하며 늦은 구 revision 응답이 최신 상태를 덮어쓰지 않도록 한다. 변경 비교에는 로그에 raw hash를 남기는 방식 대신 보호된 revision/비밀 비교 경로를 사용한다.

### 6. GitHub App 클라이언트 시크릿과 개인키 파일도 포함한다

사용자가 말한 GitHub 관련 키를 Actions Secret만으로 한정하면 안 된다. GitHub App client secret, webhook secret, PEM private key, PAT, Actions Secret 목적지는 각각 다른 역할이다. 현재 `dotenv_sync.rs:759` 원본 쓰기는 실제 개행을 가진 값을 거부하므로 여러 줄 PEM을 기존 단일 행 환경변수 수정 경로에 넣는 것만으로 해결할 수 없다.

[GitHub App 개인키 공식 문서](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/managing-private-keys-for-github-apps)에 따르면 새 개인키는 PEM 파일로 다운로드되고 GitHub는 공개 부분만 보관한다. 개인키 자체를 GitHub에서 다시 조회해 자동으로 보관하는 설계는 불가능하다. 시안에 `GITHUB_APP_PRIVATE_KEY`와 예시 PEM 원본을 추가했다. 이 항목은 실제 nomorevibe에서 관찰한 보관 항목이 아니라 추가 지원 형태를 설명하는 예시다.

**수정 방향:** 새로 발급받은 client secret은 직접 저장/선택한 환경파일에서 갱신한다. 개인키는 사용자가 지정한 PEM 파일을 보관 대상에 연결하고 파일 교체를 감지해 Keychain 보관본도 갱신한다. 앱이 다른 파일을 임의로 탐색하거나 GitHub에서 키를 재발급하지 않는다. 초기 가져오기·새 파일 재연결 흐름을 제공한다. 프로젝트 앱 ID/키 종류/파일 경로·소유권을 기록하고 다른 프로젝트 키와 섞지 않는다.

빌드용 전달은 프로젝트가 요구하는 형태에 맞춰 선택한다: secret env, 파일 경로, 실행 기간 동안만 존재하는 제한 권한 임시 파일. 임시 파일은 자식 프로세스 종료 후 정리한다. 임의의 PEM을 `.env`에 그대로 덧붙이지 않는다. provider에서 새 키가 생성됐다고 앱이 알았더라도 값을 받지 못했다면 `새 값 등록 필요`를 표시한다. 실제 키 유효성·fingerprint 비교는 별도 검증이며 보관 성공만으로 인증 성공이라고 표시하지 않는다.

## 제안 UI 계약

- 상단: 프로젝트 선택, 개발/운영 선택, 원본 파일 순서, 연결한 파일 변경 자동 보관 토글.
- 목록: 이름, 기본 숨김 키 값, 원본/사용처, 보관 상태·버전, **수정 / 복사**.
- 값 더블클릭: 실제 값 복사 intent. 이름 클릭: 상세. 행 전체 더블클릭과 경쟁하지 않게 영역을 분리한다.
- 상세: 원본, 보관 버전, 사용처, GitHub의 정확한 목적지와 전송 버전, 변경 기록, 실패/충돌 해결.
- 수정 창: 이름 고정(이름 변경은 추후 별도 계약), 새 값(빈 값=기존 유지), 변경 대상 원본, 사용처. 파일 쓰기 실패/Keychain 실패를 저장 성공으로 표시하지 않는다.
- UI 단계: `미보관`, `보관 완료`, `갱신 필요`, `Keychain 잠김`, `충돌`; 원격은 별도로 `미연결`, `반영 대기`, `전송 완료`, `전송 실패`, `외부 변경 · 값 미확인`.
- 공용 컴포넌트 재사용: `docs/design/popup-components.md`, 연결된 43개 사례의 `credential_edit`를 읽었다. 실제 구현 시 `ui/popup/`를 재사용한다. 시안 팝업 폭 480px, 입력 36px, 버튼 34px, 목록 버튼 28px, 목록 행 공통 변수 44px. 이 HTML은 새 제안이며 기존 번호별 팝업 구현을 변경하지 않았다.

## PR 단위 계획

| PR | 범위 | 의존 | 완료 기준 |
| --- | --- | --- | --- |
| 1 | 통합 목록, dotenv/수동 API 수정, 키 복사 | 없음 | 기존 파일 키 수정·복사, 값 비우면 유지, 중복 preflight와 최종 DB 검증, keyring 실패/잠금 안내, 참조 ID 유지·프로젝트 격리 |
| 2 | 원본 모델·환경 범위·PEM 파일·독립 변경 감시 | PR 1 모델 | inactive/외부/agent 파일 편집·PEM 교체·atomic rename·잠자기·재시작, last-good 보존, 변경 순서·충돌·중복 이벤트 |
| 3 | 빌드 연결·snapshot·사용 기록 | PR 2 | stale shell의 연결된 build도 최신값 사용, 실행 중 snapshot 유지, 필수값 누락·충돌 보류, agent 인증 범위 분리 |
| 4 | GitHub Secret 연결·암호화 전송·outbox | PR 2 | scope/repo/env 격리, auth/네트워크 실패와 재시도, late ack/revision fencing, remote 값 조회 불가·외부 변경 상태 |
| 5 | end-to-end harness·화면 QA·문서 | PR 1–4 | 격리 keyring/DB/fake remote + 전체 사용자 흐름·비밀값 미노출·재시작 복구·키보드/IME 테스트 |

PR 3과 PR 4는 PR 2의 타입/원본/상태 계약을 고정한 뒤 다른 소유 파일로 병렬 구현할 수 있다. 공통 `app.rs` composition root는 통합 담당자가 한 번에 연결해 충돌을 줄인다. 제품 변경·재빌드 전달은 현재 요청 범위가 아니며 이후 전달 시 version bump와 번들 검증이 필요하다. 앱 재시작은 그때의 명시적 요청으로만 수행한다.

## 이번 검증

- 실제 실행: 소스 검색/읽기, HEAD/버전 확인, 실행 경로 확인, 읽기 전용 DB 메타데이터 조회.
- HTML inline script `node --check /private/tmp/deppy-project-keys-proposal-script.js`: exit 0.
- 실제 Chrome CUA에서 합성 데이터로 확인: 숨김 키 값 더블클릭 → 복사 상태(클립보드 미변경), 기존 키 수정 폼 → 새 값 저장(v3→v4), 원본 변경 자동 보관(v4→v5), 충돌 중 빌드 보류 및 파일 기준 해결, 로컬 보관 유지/GitHub 전송 실패 표시, 연결 복구·재시도 완료, 빌드 사용 이름·버전 표시. 제품 backend 테스트를 의미하지 않는다.
- 시안에서 충돌 중 빌드 확인을 누르면 해결 버튼이 사라지는 문제를 발견해 충돌 notice를 일반 status와 분리했고, 해결 버튼 유지와 충돌 해결을 다시 확인했다. 프로젝트/환경별 감시 토글 상태도 분리했다.
- HTMLParser 검사 실제 실행: 중복 HTML ID 없음, 로컬 보고서 링크 존재, `localStorage`/`fetch`/`navigator.clipboard` 사용 없음. `node --check` 재실행 exit 0. `git diff --check` exit 0(새 문서는 untracked이므로 HTML 검사와 함께 기록).
- 데스크톱 Chrome 스크린샷으로 표·상세·버튼·PEM 예시 배치를 확인했다. 390/800/1440px viewport 자동 계측은 실행하지 않았다. 브라우저 provider(iab/chrome)가 이 환경에서 제공되지 않아 native Chrome을 사용했고, 일부 연속 자동화는 사용자 화면 변경/AX 대상 변화 때문에 검증 실패·미확인으로 두었다. 검증 완료로 적은 전송 실패/재시도/빌드 확인은 최신 AX에서 각각 다시 실행한 결과다. 사용자 Chrome 작업을 더 방해하지 않도록 추가 UI 조작을 중단했다.
- Rust 제품 코드 변경이 없으므로 이번 조사에서 Cargo 테스트/빌드·실키 rotation·GitHub write를 실행하지 않았다.

## 남은 일

제품 구현은 PR 1부터 시작한다. 화면 제안 및 실제 기능 동작의 범위를 구분하고, 특히 GitHub 외부 변경값 자동 회수와 이미 실행 중인 프로세스 환경 변경을 가능하다고 설명하지 않는다.
