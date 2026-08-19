# 사이드바 세션 라벨이 다른 워크스페이스 이름으로 보이는 문제 — 원인과 수정 (2026-08-19)

## 증상

사이드바에서 **Crawler** 워크스페이스를 펼쳤더니 그 안의 세션이 **「Design · 실행
중」**으로 표시된다(사용자 보고). 사용자는 "다른 워크스페이스의 세션이 들어갔다"고
느꼈다.

## 확정한 원인 — 세션은 안 섞였다, cwd 프로젝트명이 워크스페이스 이름 자리를 차지했다

읽기 전용 DB 조회로 세션 소유는 정확함을 먼저 확인했다:

| 워크스페이스 | pane 수 | 세션 |
| --- | --- | --- |
| Crawler (`b5a2aba3…`) | 1 | `e2ed12f4…` |
| Design (`f820fa9f…`) | 2 | `73be3307…`, `ae60678e…` |

문제는 세션 `e2ed12f4…`의 **cwd**가 `/Users/jr/.../colon35/Design`이라는 데 있다 —
`workspace_id`는 그대로 Crawler인데, cwd 폴더명이 우연히 **다른 실제 워크스페이스의
이름과 같은 단어**였다.

`WorkspaceUi::resolve_session_title`(`crates/app/src/ui/workspace.rs`)과
`session_project_context`는 cwd에서 뽑은 프로젝트명을 **단독으로** 반환했다 — 그
프로젝트명이 어느 워크스페이스 소속인지 나타내는 정보가 함께 붙지 않았다. 사이드바
세션 행은 이 값을 헤드라인으로 그대로 그린다(`file_tree.rs::session_title_lines` —
에이전트 행은 `status_line`이 headline이고, 그 `status_line`도 아직 요약/지시문이
없으면 같은 프로젝트명으로 떨어진다 `agent_activity_line`). 그 결과 텍스트만 보면
워크스페이스 카드 헤더(같은 글자 "Design")와 구분되지 않았다.

## 결정한 라벨 규칙

**cwd 프로젝트명 == 이 워크스페이스 자체 이름(`WorkspaceUi::project_name`, 별칭 없으면
루트 폴더명)이면 → 프로젝트명만 그대로 보여준다(가장 흔한 경우 — 워크스페이스 루트에서
바로 작업 중이라 정보 중복이 없다, 기존과 동일).**

**다르면 → `"{프로젝트명} ({워크스페이스명})"`으로 소속을 함께 밝힌다** — 예:
`Design (Crawler)`. cwd 기반 프로젝트명 자체는 지우지 않는다(다른 폴더에서 띄운
세션을 구분하는 원래 가치는 유지), 다만 그 이름이 워크스페이스 정체성인 것처럼 단독
표시되지 않게 한다.

이 표기 형식(`프로젝트명 (워크스페이스명)`)은 새로 만든 게 아니라 이미
`App::attached_workspace_title`(app.rs, 다른 워크스페이스 세션을 "옆에 열기"할 때 쓰는
탭 제목)이 쓰는 관례를 그대로 재사용했다 — 사용자가 이미 같은 패턴을 다른 화면에서
본 적이 있다.

### 검토한 다른 후보와 기각 이유

- **cwd 프로젝트명을 아예 안 쓰고 워크스페이스 이름만 쓴다**: 다른 폴더에서 띄운
  세션을 구분하는 기능 자체가 없어진다 — 값어치를 죽이지 말라는 요구와 충돌해 기각.
- **다르면 프로젝트명 대신 에이전트 종류/셸 이름을 보여준다**: 같은 워크스페이스 안의
  서로 다른 폴더 세션들이 전부 "Codex"/"셸"로만 보여 구분이 안 된다 — 마찬가지로
  값어치를 죽여서 기각.
- **다르면 항상 두 이름을 붙인다(같아도)**: `attached_workspace_title`은 실제로 이
  방식이다(테스트가 "Deploy (Deploy)" 중복도 허용) — 하지만 그건 "다른 워크스페이스
  세션을 옆에 열었다"는 걸 사용자가 이미 아는 화면이라 중복이 괜찮다. 사이드바 세션
  행은 2026-08-11에 "제목이 워크스페이스 이름과 같은 경우가 많아 정보가 0인 줄이
  있었다"는 사용자 피드백으로 이미 한 번 줄을 줄인 이력이 있다(같은 파일의
  `session_title_lines` 주석) — 매번 반복하면 그 판단과 어긋난다. 그래서 **같으면
  생략, 다르면만 표기**로 갈랐다.

## 「워크스페이스 목록 vs 프로젝트 목록 독립」 원칙과의 관계

이 저장소에는 **작업 워크스페이스 목록(사이드바)**과 **「환경 및 API」 프로젝트
목록(설정)**이 독립 도메인이라는, 사용자가 명시적으로 확정한 원칙이 있다
(`crates/app/src/config.rs`의 `closed_workspace_ids` vs `hidden_env_project_ids`,
PR #110에서 설정의 「워크스페이스」 관리 화면을 지운 배경).

확인 결과 **이 원칙은 이번 버그와 같은 도메인이 아니다** — `closed_workspace_ids`/
`hidden_env_project_ids`는 사이드바 워크스페이스 목록과 설정 화면의 자격증명/환경변수
프로젝트 목록 사이의 등록·삭제·이동 전파 금지를 다룬다. 이번 버그의 "cwd 프로젝트명"
(`session_name_style`, `SessionProjectNameSnapshot`)은 그 두 목록 어느 쪽과도 무관한
**세 번째 개념**(세션 표시줄 안에서 워크스페이스 자체 이름과 cwd 폴더명이라는 두 문자열이
같은 자리를 놓고 충돌하는 문제)이다. 문자 그대로 같은 원칙이 적용되는 사례는 아니다.

다만 **정신은 같은 방향**이다 — "한 목록/개념의 정체성이 다른 목록/개념의 표시 자리를
조용히 대체해서는 안 된다"는 원칙의 일반형이 이번에도 적용된다: cwd 프로젝트명(파일시스템
도메인)이 워크스페이스 이름(워크스페이스 도메인) 자리를 아무 표식 없이 차지했던 게 근본
원인이었다. 이번 수정은 대체를 막는 대신 **둘 다 보이게** 해서 어느 쪽 정체성도 지우지
않는 쪽을 택했다.

## 수정

`crates/app/src/ui/workspace.rs`:

- `qualify_cwd_project_name(&self, project_name: &str) -> String` 헬퍼 추가 — cwd
  프로젝트명과 `self.project_name`(워크스페이스 자체 표시명)을 비교해 다르면
  `"{project_name} ({workspace})"`로 합친다.
- `resolve_session_title`의 우선순위 ②(cwd 프로젝트명 스냅샷 적중) 분기가 `n.to_owned()`
  대신 `self.qualify_cwd_project_name(n)`을 반환하도록 변경.
- `session_project_context`(에이전트 행 headline의 project_context 소스)도 같은 규칙을
  적용 — 스냅샷 적중/파일시스템 basename 폴백 두 경로 모두. 반환 타입이 `Option<&str>`
  에서 `Option<String>`으로 바뀌어(합성 문자열은 self를 빌릴 수 없다) 유일한 production
  호출부(`session_entries`)에서 `project_context.as_deref()`로 넘기도록 함께 고쳤다.

`self.project_name`(워크스페이스 자체 이름)은 활성 워크스페이스로 있는 동안 매 프레임
`set_project_name`으로 갱신되고, warm으로 내려갈 때 `WorkspaceUi` 전체가 그대로
`self.warm`으로 옮겨져(app.rs의 활성→warm 전환 경로, `set_project_name`/
`set_session_project_names` 호출부를 리셋하지 않음) 마지막 값이 유지된다 — 그래서 이
비교는 활성·warm 워크스페이스 모두에서 새 배선 없이 성립한다. 이 gate 리포트 작성
시점에는 app.rs를 건드리지 않고도 성립함을 코드 추적으로 확인했다(런타임으로 재확인은
못함, 아래 미검증 참고).

폭 부족 시 잘림은 손대지 않았다 — 헤드라인은 기존과 동일하게
`file_tree.rs::clipped_line`(1행, `overflow_character: '…'`)을 그대로 거친다. 다만
아주 좁은 폭에서는 `"Design (Crawler)"`가 `"Design (Craw…"`처럼 잘려 워크스페이스명
쪽이 일부만 보이거나, 극단적으로 좁으면 괄호 전부가 잘려 원래의 모호한 형태로
되돌아갈 수 있다 — 단일행 말줄임의 본질적 한계라 이번 수정 범위에서 별도 처리는
하지 않았다.

## 테스트 (1차)

`crates/app/src/ui/workspace.rs` 테스트 모듈에 추가:

- `cwd_project_name이_워크스페이스_자체_이름과_다르면_소속을_함께_보여준다` — 사용자
  보고를 그대로 재현(Crawler 워크스페이스, cwd가 Design 폴더)해 `resolve_session_title`과
  `session_project_context`가 둘 다 `"Design (Crawler)"`를 반환하는지 검증.
- `cwd_project_name이_워크스페이스_자체_이름과_같으면_그대로_보여준다` — 워크스페이스
  루트에서 작업 중인 흔한 경우 회귀 방지(정보 중복 없이 프로젝트명 단독 유지).

기존 테스트 `precomputed_project_name_snapshot은_exact_session_cwd에만_적용된다`는
`session_project_context`의 반환 타입 변경(`Option<&str>` → `Option<String>`)에 맞춰
어서션만 `.to_owned()`로 고쳤다 — 검증 내용은 그대로다.

## 2차 수정 (코드 리뷰 반영) — `activity_session_name` 경로

1차 수정 직후 코드 리뷰에서 **사용자가 실제로 본 화면은 아직 안 고쳐졌다**는 지적을
받았다. 스크린샷 상황은 Design이 활성, Crawler는 접힌 **warm(비활성)** 워크스페이스 —
그 Crawler 아래 나열된 세션 행이 문제였다. warm/유휴 워크스페이스의 세션 행은
`WorkspaceUi::resolve_session_title`을 전혀 거치지 않는다 — 감지 워커가 활성
워크스페이스에서만 돌기 때문에, warm/유휴 행은 App(`app.rs`)의 별도 순수 함수
`activity_session_name`(`:28469` 부근 자유 함수, `:21222` 부근 `&self` 래퍼)이 만든다.
1차 수정은 이 경로를 건드리지 않아 정확히 사용자가 본 행이 미해결로 남아 있었다.

### 규칙 중복을 피한 방법

`qualify_cwd_project_name`의 본문(같으면 그대로, 다르면 `"프로젝트명 (워크스페이스명)"`)
을 workspace.rs의 새 **모듈 자유 함수** `pub(crate) fn qualify_project_name(project_name:
&str, workspace_name: Option<&str>) -> String`로 뽑았다(`display_pane_title` 바로
옆 — 이미 그 주석이 "활성 경로/활동 패널 warm·유휴 행이 같은 규칙을 쓴다"고
전제하고 있던 자리다). `WorkspaceUi::qualify_cwd_project_name`은 이제
`self.project_name`을 넘기는 얇은 래퍼일 뿐이다.

App(`app.rs`)은 leaf(`ui::workspace`)를 참조해도 되지만 leaf는 App을 참조하면 안
되는 방향성(`xtask check-boundary`가 지키는 leaf/App 경계와 같은 방향)이라, 공유
장소는 workspace.rs(leaf) 쪽으로 정했다. `app.rs`의 자유 함수 `activity_session_name`은
`workspace_name: Option<&str>` 파라미터를 새로 받아 `ui::workspace::qualify_project_name`을
그대로 호출한다. `&self` 메서드 `App::activity_session_name`은 `self.workspaces`에서
`Self::workspace_display_name`(이미 있던 함수, `active_workspace_display_name` 등이
같은 방식으로 쓴다)으로 워크스페이스 이름을 찾아 넘긴다. 이 메서드가 유일한
production 호출부이므로, **호출부 코드는 한 곳도 안 바꾸고** 6곳 모두가 자동으로
같은 규칙을 상속한다.

### 호출부 7곳 판단

`self.activity_session_name(workspace_id, raw_title)` 호출부는 7곳이다(자유 함수
정의·테스트 제외):

| 위치 | 화면 | 판단 |
| --- | --- | --- |
| `app.rs:15303`(`web_workspace_seed`, warm) | 폰/웹 대시보드 | 포함 — 데스크톱과 같은 원천·규칙을 쓴다고 그 함수 자신의 doc이 명시. 메서드 상속으로 자동 반영. |
| `app.rs:15320`(`web_workspace_seed`, 유휴) | 폰/웹 대시보드 | 포함 — 위와 동일. |
| `app.rs:21265`(`dispatch_storm_notifications`) | macOS OS 알림(osascript) | 포함 — i18n 템플릿(`process_storm.notification.body`)에 워크스페이스 이름이 전혀 없어(세션명·개수만) 다른 화면보다 오히려 더 절실하다. 확인해 보니 이 템플릿에 워크스페이스 문맥이 아예 없었다. |
| `app.rs:21976`(`activity_rows`, warm) | **Activity 패널** — 코드 리뷰가 지목한, 사용자가 실제로 본 화면 | 포함 — 최우선 수정 대상. |
| `app.rs:22032`(`activity_rows`, 유휴) | Activity 패널 | 포함 — 위와 같은 함수의 다른 분기, 일관성. |
| `app.rs:24430`(사이드바, cold/미warm 워크스페이스 `SidebarSessionRow::from_persisted_parts`) | 사이드바 트리(1차에서 고친 warm 경로의 cold 대응) | 포함 — 1차에서 고친 `resolve_session_title` 경로(활성·warm)와 짝을 맞춰야 사이드바 안에서 표기가 갈리지 않는다. |
| `app.rs:13438`(`apply_resume_probe_results`, resume-missing 알림 문구) | "이어서 하기" 세션을 못 찾았다는 안내 문구 | **코드는 건드리지 않음** — 이 지시의 "이어서 하기/codex resume 배선은 다른 에이전트 소유" 규칙 때문에 이 파일 영역엔 손대지 않았다. 다만 공유 메서드를 통해 자동으로 같은 규칙을 상속한다(호출부 코드 자체는 무변경) — 이 알림도 워크스페이스 소속을 함께 밝히는 게 유해하지 않다고 판단해 상속을 막지 않았다. |

7곳 전부 "워크스페이스 소속을 밝히는 게 부적절한 자리"는 없었다 — 제외한 곳은 없다.
(리뷰 메시지가 "6곳"이라 적었으나 실제로는 7곳이었다 — `24416`이 누락돼 있었다.)

### 테스트 (2차)

- `crates/app/src/app.rs`에 `활동_pane_이름은_워크스페이스_자체_이름과_다르면_소속을_함께_보여준다`
  추가 — 자유 함수 `activity_session_name`에 `Some("Crawler")` workspace_name과 cwd
  `"/projects/colon35/Design"`을 넣어 `"Design (Crawler)"`를, 워크스페이스 루트
  cwd(`"/projects/Crawler"`)에서는 `"Crawler"` 그대로를 검증 — 사용자가 본 정확한
  시나리오의 재현이다.
- 기존 `활동_pane_이름은_기본제목이면_프로젝트명으로_표시된다`는 새 `workspace_name`
  파라미터(`None`)만 추가해 기존 검증 내용을 그대로 유지했다.
- `App::activity_session_name`(`&self` 메서드, `self.workspaces`에서 이름을 찾는
  배선)은 별도 테스트를 추가하지 않았다 — 이 코드베이스가 이미 "순수 — 테스트 대상"
  이라고 자유 함수를 따로 뽑아 둔 이유가 바로 무거운 `App` 인스턴스 없이 테스트하기
  위해서였다(자유 함수 doc 주석). 메서드는 `self.workspaces.iter().find(...).map(
  Self::workspace_display_name)`뿐인 얇은 배선이고, 이미 `active_workspace_display_name`
  등에서 검증된 것과 같은 패턴이라 기존 관례를 따라 자유 함수 테스트로 커버 범위를
  한정했다.

## 게이트 (2차 수정 후 최종)

- `cargo test -p deppy-sijo`: 1796 passed, 0 failed, 11 ignored (workspace.rs 신규
  2개 + app.rs 신규 1개 포함).
- `cargo clippy --workspace --all-targets -- -D warnings`: 0 경고.
- `cargo run -q -p xtask -- check-boundary`: OK.
- i18n 미변경 — `i18n-check`/`cargo test -p i18n` 스킵(가이드 조건에 따라).
- `cargo fmt --all -- --check`: `app.rs` 37곳, `workspace.rs` 7곳 — 둘 다 스태시 대조로
  **브랜치 시작 전부터 있던 기존 drift와 정확히 같은 개수**임을 확인했다(2차 수정 중
  한 줄이 길어져 새 drift가 1곳 생겼던 것을 rustfmt가 원하는 형태로 직접 고쳐 0으로
  되돌렸다).

## 미검증 (실제 화면으로 확인 못함)

- 실제 앱에서 Activity 패널의 Crawler(warm) 아래 세션이 `Design (Crawler)`로 보이는지는
  화면으로 확인하지 못했다 — 오케스트레이터의 최종 빌드·실행 확인이 필요하다(작업
  지시에 따라 이 워크트리에서 앱을 직접 빌드/실행하지 않았다).
- 좁은 사이드바/Activity 패널 폭에서 실제 잘림 형태(위 "폭 부족 시 잘림" 절)도 육안
  확인이 필요하다.
- cwd가 OSC 타이틀(활성 워크스페이스 우선순위 ③)로만 감지되는 경로는 여전히 미적용
  — `resolve_session_title`의 그 분기는 이번에도 건드리지 않았다. 사용자가 보고한
  정확한 사례(활성 경로의 cwd 프로젝트명 스냅샷, warm 경로의 `activity_session_name`)
  는 이제 둘 다 커버되지만, OSC 타이틀 경로의 동일 증상은 이번 범위 밖으로 남겨
  기록만 한다.
- `app.rs:13438`(resume-missing 알림)은 코드를 건드리지 않았지만 공유 함수를 통해
  동작이 바뀐다 — resume 배선을 소유한 다른 에이전트가 이 부수 효과를 알아야 할 수
  있어 여기 기록한다.
