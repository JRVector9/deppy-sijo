# 에이전트 터미널 표 크기 변경 조사

- 브랜치: `feat/agent-command-center-v2-20261020`; 기준 commit: `7584ed75fdc5949647a58574369c29606a076a86`; 소스 버전 0.15.1.
- 사용자 범위: 에이전트 터미널에 출력된 표. Claude·Codex 모두 발생한다고 확인했고 Grok도 테스트 요청했다.
- **Grok 실제 CLI 테스트 완료. 제품 수정 완료는 아니다.** Claude classic의 과거 출력에서 고정 너비를 재현했다. Codex는 합성 사례에서 정상 재배치되어 실제 보고 사례는 미확정이다.
- 사용자 앱 재시작, 사용자 세션 입력·프롬프트 제출, 유료 모델 호출, 실제 자격 증명 복사, 제품 코드/버전 변경은 하지 않았다.

## 실제 CLI 결과

macOS에 설치된 CLI의 합성 저장 대화를 실제 Deppy `Session` → PTY → Alacritty 경로로 복원했다. 40행을 유지하며 80 → 42 → 120 → 80열로 변경했다. 외부 IP 네트워크를 sandbox로 차단하고 각 CLI의 별도 임시 설정/대화 및 가짜 키만 사용했다. GUI pane 배치/resize 큐를 포함한 앱 전체 E2E는 아니다.

과거 출력 테스트에서는 표 다음에 별도 사용자/에이전트 턴과 40개 문단을 추가하여 표가 live viewport 위로 지나가게 했다. 전체 화면 모드는 CLI 자신의 PageUp/PageDown으로 과거 대화를 관찰했다. 이 키는 테스트 소유 CLI에만 전송했다.

| CLI / 버전 | 모드 | 화면에 있는 표 | 과거 출력의 표 | 관찰한 표 너비(80 → 42 → 120 → 80열) |
|---|---|---|---|---|
| Grok 1.0.67, build 2fe064390d07 | minimal | 재배치 | 재배치 | 80 → 42 → 120 → 80 |
| Grok 1.0.67 | fullscreen | 재배치 | 재배치 | 61 → 23 → 101 → 61 |
| Codex 0.163.0 | inline (`--no-alt-screen`) | 재배치 | 재배치 | 78 → 40 → 118 → 78 |
| Codex 0.163.0 | 기본 alternate screen | 재배치 | 재배치 | 78 → 40 → 118 → 78 |
| Claude 2.1.305 | classic | 재배치 | **기존 너비 유지** | live: 76 → 카드형 → 116 → 76; 과거: 80열과 120열 모두 76 |
| Claude 2.1.305 | fullscreen | 재배치 | 판정 보류 | 긴 이력은 초기 화면에 없고 onboarding dialog가 있어 미검증 |

- Grok의 한글 셀도 새 너비에 맞춰 줄바꿈된다. 표가 길어지는 42열 fullscreen에서는 전체 표가 한 화면에 보이지 않지만 top/bottom 경계 너비가 23열로 바뀌었다. viewport에 모두 안 보이는 것을 표 고정으로 판단하지 않았다.
- 사용자의 Grok 설정에서 읽은 비밀이 아닌 렌더링 항목은 `ui.screen_mode="minimal"`이다. 설정을 바꾸지 않았다. 이 모드에서도 합성된 과거 표는 정상 재배치됐다.
- Claude classic의 과거 표는 120열로 넓힌 뒤에도 최초 80열 화면에서 출력한 76열 경계를 보존했다. 과거 터미널 행이 유지되는 경우와 live Markdown이 재렌더링되는 경우를 구분해야 한다. onboarding dialog가 표시된 복원 대화에서 얻은 관찰이므로 모든 classic 상태로 확대 해석하지 않는다.
- Codex의 정상 결과는 사용자가 본 오류가 없다는 뜻이 아니다. 실제 영향을 받은 세션, 원격 CLI 버전, 표가 에이전트 답변인지 tool stdout인지, scrollback 위치 및 앱 resize 큐를 대조해야 한다. 현재 사용자 화면을 읽기 전용으로 확인했지만 문제 표는 보이지 않았다.

## 증거와 판정 방법

[합성 화면·결과 manifest](assets/agent-terminal-table-resize-20261022/manifest.json)에 CLI 버전, 단계별 표 너비, 화면 파일 경로, SHA-256을 보관했다. 화면 우측 padding 및 마지막 빈 행만 제거했다. 원래 viewport 크기는 metadata.tsv에 유지했다. 사용자 대화/키/원격 파일은 포함하지 않는다.

- `grok/minimal-verified`, `grok/fullscreen-verified`: 새로 복원한 표.
- `grok/minimal-history`, `grok/fullscreen-history`: 과거 표. fullscreen은 `application-top-*` 화면.
- `claude/classic`, `claude/classic-history`: live/과거 차이.
- `codex/inline-history`, `codex/default-history-nav`: 과거 표.

8개 증거 묶음의 32단계 중 너비를 읽을 수 있는 **30단계의 실제 경계 너비를 별도 Python 단정으로 확인**했다. 나머지 두 Claude 42열 관찰은 카드형 전환 또는 256행 이력 창 밖이어서 너비를 단정하지 않았다. 이 검사는 Claude의 고정 너비도 관찰값으로 확인하며, 제품 수정의 GREEN을 의미하지 않는다.

수동 진단 `installed_agent_table_resize_probe`의 PASS는 합성 표가 표시됐다는 뜻이다. 너비/재배치 판정은 위 별도 측정과 화면 증거를 따른다. ignored 테스트를 일반 테스트 통과 수에 합산하지 않는다.

## 터미널 경로와 수정 경계

`crates/session/tests/terminal_table_resize.rs`의 일반 테스트는 다음을 검증한다.

1. 테스트 소유 셸이 SIGWINCH를 받으면 `stty size`를 읽고 표를 다시 그린다. 80×24 → 42×20 → 120×32 → 80×24에서 자식 크기, 표 너비, snapshot 크기를 확인한다.
2. 일반 문장 soft wrap은 넓히면 합쳐지지만, 명시적 CRLF/공백으로 만든 표는 원래 열 배치를 유지한다.

실제 경로는 `workspace.rs`의 pane 폭 계산/resize 전달 → `Session::resize_checked`의 backend+PTY resize → portable-pty의 TIOCSWINSZ → Alacritty Term::resize다. Alacritty는 WRAPLINE soft wrap을 reflow하며 프로그램이 명시한 줄바꿈/공백의 의미를 재해석하지 않는다.

따라서 모든 표를 터미널 resize 실패로 단정할 수 없다. 반대로 원문을 보유하는 CLI가 표를 재렌더링하지 않은 과거 행은 VT engine만으로 열 너비를 계산할 수 없다. 임의 ANSI 셀을 추측하여 다시 쓰거나 Ctrl+L을 자동 입력하지 않았다. Markdown 문서 탭 변경으로 이 문제를 대신 해결했다고 하지 않는다.

전체 화면 강제도 아직 적용하지 않았다. Deppy의 휠 경로는 native scrollback을 사용하고 application mouse reporting은 별도 검증이 필요하다. 모드만 강제하면 스크롤·선택·입력 계약에 영향을 줄 수 있다.

## 실제 실행한 검사

| 명령/실행 | 결과 | 범위 |
|---|---|---|
| `cargo test --offline --locked -p session --test terminal_table_resize -- --test-threads=1 --nocapture` | **2 PASS, 0 FAIL, 1 ignored** | 최종 테스트 소스, PTY 왕복/고정 행 특성 |
| 명시적 격리 CLI probe 8개 증거 묶음 | 각 **1 PASS** | 표 표시 확인; 별도 너비 판정 필요 |
| `cargo test --offline --locked -p session --lib resize_checked -- --test-threads=1` | 3 PASS, 0 FAIL, 78 filtered | 이전 단계 실제 실행, 제품 소스 변경 없음 |
| `cargo clippy --offline --locked -p session --test terminal_table_resize -- -D warnings` | exit0 | 최종 테스트 소스 및 의존 검사 |
| `cargo fmt --all --check` / `git diff --check` | exit0 / exit0 | 최종 소스 형식 및 tracked diff 공백 검사 |

초기 실패는 조사 도구/fixture 실패였다: 셸 IFS와 우측 padding, Grok summary 필수 필드 누락 및 updates.jsonl 누락, Codex resume 경로 대신 UUID 필요, Grok이 ROW_ALPHA 식별자를 셀 안에서 줄바꿈한 것을 놓친 표시 단정, alternate screen 과거 이력을 native history로 찾은 오류. 이후 테스트 소유 fixture/관찰 경로만 수정했다. 이 실패를 제품 결함이나 제품 수정 RED로 계산하지 않는다. 수동 history 읽기 추가 중 잘못된 snapshot 메서드 이름으로 한 번 컴파일 실패했고 실제 메서드로 수정했다.

## 남은 작업 / 완료 조건

1. Codex의 실제 영향 세션/표와 동일한 버전·출력 경로에서 재현한다. 사용자에게 세션 식별을 요청했다. 실제 사용자 프롬프트 제출은 하지 않는다.
2. Claude classic의 과거 출력 문제는 원문 기반 재렌더링 경로와 모드/스크롤 계약을 검토한다. 임의 VT 행 재작성은 사용하지 않는다. fullscreen 과거 표는 onboarding을 해소한 테스트 소유 세션에서 추가 검증이 필요하다.
3. 실제 pane resize 큐를 포함해 동일 표의 좁힘/넓힘/복원, 입력·커서·선택/복사, 원격 세션을 검증한 뒤에만 제품 수정 완료로 판정한다.

## 참고한 1차 자료

- [Claude fullscreen 공식 문서](https://code.claude.com/docs/en/fullscreen): 원문 기반 전체 화면 렌더러 및 모드 전환. 사용자 설정에 적용하지 않았다.
- [Codex Markdown 원문 렌더링 소스](https://github.com/openai/codex/blob/main/codex-rs/tui/src/history_cell/messages.rs): 최신 main의 설계 자료이며 설치 버전 동작은 위 실제 테스트로 확인했다.
- [Grok 공식 소스](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/src/scrollback/blocks/markdown_content.rs): width 변경에 따른 렌더링 캐시 무효화. 이 main SHA는 설치 build 2fe064390d07과 다르다. 설치 build의 raw source URL은 404였으며 동일 소스로 주장하지 않는다.
