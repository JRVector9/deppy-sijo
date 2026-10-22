# Claude 과거 표 원문 렌더링 조사 — PR2

- 작업 브랜치: `fix/table-resize-claude-20261022`; 기준 소스 `7584ed75fdc5949647a58574369c29606a076a86`.
- 목적: onboarding을 해소하고 수동 권한 상태에서 classic 과거 표 고정 현상을 재확인한 뒤, fullscreen 전환의 입력·스크롤 계약을 판정한다.
- **과거 표 결함은 재현됐다. 자동 fullscreen 제품 변경은 적용하지 않았다.** 현재 native wheel 경로가 application mouse events를 전달하지 않아, 런처에서 모드만 바꾸면 스크롤 동작이 퇴행한다.
- 사용자 앱·설정·대화는 변경하지 않았고 앱 실행/재시작·모델 요청·사용자 세션 입력도 하지 않았다. 소스 버전 0.15.1은 변경하지 않은 조사 빌드다.

## 정제한 실제 CLI 결과

설치된 Claude **2.1.305**와 **2.1.307**의 버전별 실제 바이너리를 직접 지정했다. 조사 도중 `/Users/jr/.local/bin/claude`가 2.1.307을 가리키므로 symlink 경로에 설치 버전이 고정됐다고 가정하지 않았다. 바이너리 경로·SHA-256·실행 인수는 각 사례의 `provenance.json`에 있다.

각 실행은 독립된 임시 profile/project와 합성 user/assistant 4개 레코드를 사용했다. 첫 응답의 표 이후 별도 턴에 40개 문단을 두었다. inherited provider credential env를 제거하고 dummy key만 제공했으며 `sandbox-exec`으로 외부 IP 연결을 차단했다. `--bare`, 비활성 setting sources, 빈 strict MCP, updater/비필수 트래픽 비활성화, **`--permission-mode default`**를 사용했다.

기존 실제 `Session`/PTY/Alacritty 라이브러리를 읽기 전용으로 연결한 별도 Rust probe가 40행을 유지하며 80 → 42 → 120 → 80열로 변경했다. 공유 소스와 target은 수정하지 않았다. 기존 dialog에서 Down을 단독 전송하고 최소 350ms pump한 다음 화면에 **`❯ No, keep manual mode`가 확인됐을 때만 Enter를 한 번** 전송했다. 두 버전의 모든 최종 화면에서 onboarding과 `auto mode on`이 없고, 마지막 합성 transcript는 `permission-mode: default`였다.

| CLI | Renderer | 과거 표: 80 → 42 → 120 → 80열 | 이력 경로 |
|---|---|---|---|
| 2.1.305 | classic | 76 → 측정 불가 → **76** → 76 | native history |
| 2.1.305 | fullscreen | 76 → 카드형 → **116** → 76 | CLI PageUp/PageDown |
| 2.1.307 | classic | 76 → 측정 불가 → **76** → 76 | native history |
| 2.1.307 | fullscreen | 76 → 카드형 → **116** → 76 | CLI PageUp/PageDown |

42열 classic의 조각 난 경계는 완전한 표 경계로 측정하지 않았다. fullscreen은 ROW_ALPHA/ROW_OMEGA와 Status를 가진 카드 형태로 바뀐다. fullscreen 과거 표는 native scrollback이 아니라 CLI의 PageUp으로 관찰했다. 모든 크기에서 PageDown은 `END_OF_HISTORY_FIXTURE`와 최신 입력 프롬프트로 돌아왔다.

별도 2.1.307 실행에서는 **제출하지 않은 `한글 초안 DRAFT_KEEP`**가 classic/fullscreen 모두 네 크기에서 보존됐다. fullscreen에 SGR wheel up/down 이벤트를 직접 입력하면 과거 화면으로 이동하고 최신 이력으로 복귀했으며, wheel과 PageUp/PageDown 모두 초안을 보존했다. 실제 raw 출력은 fullscreen에서 mouse 1000/1002/1003 및 SGR 1006 enable을 요청했다. classic은 이 mouse 모드를 요청하지 않았다.

[측정 결과](assets/claude-table-source-20261022/measurements.json)와 [SHA-256 manifest](assets/claude-table-source-20261022/manifest.json)에 6개 정제 실행, 24개 크기 단계의 화면·상태·메타데이터가 있다. 화면의 오른쪽 terminal padding과 마지막 빈 행만 제거했으며 manifest에 원본 capture SHA도 기록했다. 각 probe의 1 PASS는 표 표시/해당 입력 단정을 뜻하며 classic resize 성공을 뜻하지 않는다. 별도 Python 단정은 완전한 경계의 너비, 카드 형태, modal/auto 표시 부재를 실제로 확인했다.

## 원인과 제품 변경 조건

classic은 이미 출력한 과거 표를 명시적 열·줄바꿈을 가진 native terminal history에 둔다. 화면을 넓혀도 최초 76열 표가 남는다. 따라서 이 사례는 onboarding 또는 PTY 크기 전달 실패가 아니다. fullscreen은 이전 메시지 원문을 자기 화면에 다시 그리므로 120열에서 116열 표가 된다.

`agent_launcher::build_launch_spec`에는 설치 버전·renderer capability 협상 경로가 없다. `CLAUDE_CODE_NO_FLICKER=1`을 무조건 넣는 변경은 사용자가 저장한 classic 선택을 덮어쓰고 CLI의 실패한 fullscreen 시작 이후 fallback도 우회한다. 현재 Deppy `Session::scroll`은 native `scroll_display`를 호출하며, runtime Scroll은 view operation이고 WriteInput은 별도 승인 경로다. Scroll을 몰래 PTY 키로 바꾸면 안 된다.

자동 기본값을 바꾸기 전에 필요한 계약은 다음과 같다.

1. 지원되는 CLI/renderer capability와 사용자의 명시적 `tui`·`CLAUDE_CODE_*` 설정 우선순위를 보존한다. 원격·구버전 지원을 추정하지 않는다.
2. application mouse 요청 상태를 UI까지 전달하고, wheel 이벤트를 일반 권한 검사를 거치는 WriteInput으로 보낸다. native scroll view command 자체를 입력으로 변경하지 않는다.
3. native selection/copy 우회 키, 한글 입력·커서, 최신 프롬프트 복귀와 remote control 권한을 실제 앱 경로에서 검증한다.
4. 이후 해당 계약의 RED launcher/scroll/input 회귀를 먼저 실행하고, 승인·model·effort 인수를 보존하는 최소 제품 변경을 별도 커밋으로 한다.

[Claude 공식 문서](https://code.claude.com/docs/en/fullscreen)는 fullscreen의 alternate buffer/application scrolling, mouse reporting 필요, native selection 우회 및 설정 우선순위를 설명한다. 위 설치 버전의 실제 동작은 문서 추정 대신 합성 CLI 실행으로 확인했다. 앱의 wheel·selection/copy·cursor E2E, remote CLI, streaming 중 표는 이번 하위 조사에서 검증하지 않았다.

## 초기 실패와 증거 정정

- 최초 private clone 경로에 `2.1.305`를 넣었을 때 Claude의 project 경로 인코딩이 달라 resume lookup이 실패했다. 새로 생성한 별도 경로의 버전 구분자를 hyphen으로 변경했다. 제품 RED가 아니다.
- 원래 mutable 합성 transcript는 user/assistant 이외에 **`permission-mode: auto` 메타데이터를 이미 포함했다**. 원본 SHA-256은 `3223b0854c8608932a3e91373eef9ca37888774a4980c24bc39ee625badeb9ec`이며 [정정 provenance](assets/claude-table-source-20261022/original-source-permission-provenance.json)에 원본과 조사 clone의 같은 레코드를 남겼다. 처음 No를 선택한 clone도 이 metadata를 복원해 auto indicator가 남았다. 이 실행들은 정제 비교에서 제외했다. 기존 보고서의 자동 권한을 켜지 않았다는 포괄적 표현은 해당 test-owned fixture의 상태와 구분해야 한다. 사용자 설정·제품 실행에는 영향이 없다.
- 새 실행에서는 4개의 합성 메시지만 복사하고 explicit default permission을 사용했다. modal 선택을 해소한 뒤 실제 prompt와 저장 metadata가 모두 manual/default인 것을 확인했다.
- private input probe를 확장하다 String을 파일에 move한 뒤 재사용해 E0382로 1회 컴파일 실패했다. 파일 write에 reference를 넘겨 수정했다. 바이너리가 없던 두 실행 시도도 fixture 실패이며 실제 CLI 테스트로 세지 않았다.

## 실제 명령과 결과

```sh
# /private/tmp/deppy-claude-table-pr2-20261022/probe{,-input}.rs를
# 아래 exact rlib들로 rustc --test 컴파일했다. 공유 target은 읽기 전용이었다.
rustc --edition=2021 --test /private/tmp/deppy-claude-table-pr2-20261022/probe.rs \
  -o /private/tmp/deppy-claude-table-pr2-20261022/probe \
  -L dependency=/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020/target/debug/deps \
  --extern session=/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020/target/debug/deps/libsession-97bad97f50775ea1.rlib \
  --extern terminal=/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020/target/debug/deps/libterminal-94d29267beb44ad0.rlib \
  --extern pty=/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020/target/debug/deps/libpty-63372cd37e55e12e.rlib \
  --extern deppy_core=/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020/target/debug/deps/libdeppy_core-ebb21993fb02ce2c.rlib
python3 /private/tmp/deppy-claude-table-pr2-20261022/run.py classic 2.1.305
python3 /private/tmp/deppy-claude-table-pr2-20261022/run.py fullscreen 2.1.305
python3 /private/tmp/deppy-claude-table-pr2-20261022/run.py classic 2.1.307
python3 /private/tmp/deppy-claude-table-pr2-20261022/run.py fullscreen 2.1.307
# probe-input.rs도 위와 동일한 rlib 인수로 컴파일, 출력은 probe-input.
python3 /private/tmp/deppy-claude-table-pr2-20261022/run-input.py classic 2.1.307
python3 /private/tmp/deppy-claude-table-pr2-20261022/run-input.py fullscreen 2.1.307
```

- 각각 exit0, **1 PASS**. 경과 시간 순서대로 14.16s, 22.24s, 14.12s, 22.19s, 14.65s, 25.99s.
- `python3 docs/qa/assets/claude-table-source-20261022/assert-evidence.py`: exit0, 24단계 hash/완전 경계/카드/modal/auto/default permission 단정 PASS. `git diff --cached --check`: exit0. classic fixed width를 관찰대로 확인한 것이며 제품 수정 GREEN이 아니다.
- 이 probe 코드와 runner 사본은 조사 provenance다. 일반 재실행 가능한 provider fixture 생성/실패 전파/analysis gate는 별도 PR3 소유다. shared fixture를 다시 사용하거나 기존 mutable permission metadata를 복사하지 말 것.

## 다음 작업 / 정확한 인계

- `git status --short`; `git diff --check`; 이 문서와 `assets/claude-table-source-20261022/{manifest,measurements}.json`을 읽는다.
- root와 PR3 담당자는 clean4-message + default permission + split/confirmed No bootstrap을 durable harness에 반영한다.
- root가 application mouse와 native selection 계약을 통합하기 전까지 런처 인수·환경을 바꾸지 않는다. 이 조건은 사용자 승인 요청이 아니라 제품 계층 간 구현 조건이다.
- 제품 코드가 변경되지 않았으므로 bugfix RED/GREEN, 앱 릴리스, 전체 workspace gate 또는 앱 UI 회귀 통과를 주장하지 않는다.
