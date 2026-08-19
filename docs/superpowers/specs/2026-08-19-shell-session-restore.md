# 셸(SSH) 세션 화면이 앱 재시작 후 사라지는 문제 — 원인과 수정 (2026-08-19)

## 증상

"프로그램이 종료되어도 다시 시작할때 ssh 세션 화면이 그대로 남아있어야해. 지금은 다
사라지고있어" — 원격 서버에 SSH로 붙어 작업하다가 앱이 재시작되면 그 pane의 화면이
사라진다.

## 조사: "이미 확정된 원인"을 재확인한 결과

작업 지시서는 `crates/runtime/src/in_process.rs:2421` 주석("agent였던 pane은
respawn 대신 열람 전용 복원(PR-A2)")을 근거로 "셸은 respawn되어 화면이 초기화된다"를
확정 원인으로 제시했다. 코드를 실제로 추적하니 **이 설명은 부분적으로만 맞는다**:

- `restore_pane`(`in_process.rs:2432` 부근)은 실제로 kind가 `agent`인 pane만
  `restore_archived_pane`(열람 전용)으로 보내고, 셸은 fresh PTY를 spawn한다 — 여기까진
  지시서와 일치.
- 하지만 그 직후 **셸도 `Self::replay_saved_ansi(&self.logs_root, persistent_id, &mut
  new_session)`를 호출한다**(`in_process.rs:2489` 부근) — 이전 실행이 남긴
  `redacted.ansi.log`를 새로 spawn한 세션의 terminal parser에 다시 먹여 scrollback과
  색을 복원한 뒤에 fresh 셸을 그 위에 이어 붙이는 경로가 **이미 존재했다**. 이 메커니즘은
  2026-07-10부터 있었고 `재시작시_ansi_scrollback과_color가_복원된다`
  (`in_process.rs:9566`, `SpawnShell`로 셸을 만들어 테스트) 테스트로 이미 고정돼 있다 —
  본 작업 착수 시점에 이 테스트를 실행해 통과함을 직접 확인했다.
- 셸 세션도 agent와 **동일한 코드 경로**로 로그를 남긴다: `SpawnShell` 핸들러가
  `pipe.session_spawned(..., "shell", ...)` 다음 `open_session_log(id)`를 호출하고
  (`in_process.rs:1508-1537`), 매 pump마다 `redact_chunk` → `append_redacted_output`이
  agent/셸 구분 없이 똑같이 실행된다(`in_process.rs:3062-3069`, `:3275-3281`). 앱 정상
  종료 시(`App::shutdown_on_exit` → `runtime.shutdown()`, 별도 조사로 확인 — `crates/app/
  src/app.rs`에 `std::process::exit`는 없고 warm 워크스페이스까지 전부 join한다) 남은
  출력을 `final_drain`으로 한 번 더 기록하고 `close_session_log`로 마감한다
  (`in_process.rs:1263-1298`). 로그 파일 쓰기 자체도 `BufWriter` 없이 `std::fs::File::
  write_all`을 직접 호출해(`crates/storage/src/logs.rs:93-118`) 프로세스가 죽어도(SIGTERM
  등) OS로 넘어간 바이트는 남는다 — "셸은 로그/아카이브를 안 남긴다"는 가설은 **사실이
  아니었다**.

즉 "복원 자체가 없다"가 아니라, **복원이 있는데 특정 조건에서 복원된 화면이 다시
지워진다**가 실제 버그였다.

## 진짜 원인 — alt-screen 콘텐츠가 `finish_ansi_replay`에서 버려진다

`replay_saved_ansi_ext`는 재생이 끝나면 `finish_ansi_replay()`를 불러 fresh PTY가
붙기 전 mode 경계를 정리한다(alt-screen 종료, mouse tracking 해제 등). 이 함수가
기존에 보내던 바이트:

```
\x1b[?1049l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[r\x1b[?6l\x1b[?7h\x1b[4l\x1b[0m\x1b[?25h\r\n
```

`\x1b[?1049l`은 **alternate screen을 무조건 나간다** — DEC 1049 시맨틱상 alt-screen에는
scrollback이 없으므로, 나가는 순간 alt-screen에 있던 내용은 영구히 사라지고 진입 이전의
primary 화면(대개 훨씬 오래된 내용)만 남는다. 이 동작은 실수가 아니라 의도적으로
테스트까지 있었다(`crates/session/src/session.rs`의
`ansi_replay_경계는_alt_screen을_끝내고_fresh_출력을_새줄에_둔다` — 원래 버전은
"OLD-HISTORY"만 남고 "ALT-SCREEN"은 사라짐을 **단언**하고 있었다).

SSH로 원격에 붙어 작업할 때 `vim`·`htop`·`tmux`·`less`·`man` 등은 전부 alternate
screen을 쓴다 — "SSH 세션 화면이 사라진다"는 사용자 보고와 정확히 일치하는 조건이다.
평범한 셸 프롬프트(항상 primary 화면)만 쓰던 세션은 원래도 문제 없이 복원됐다 — 이
때문에 지시서의 "셸은 항상 화면이 사라진다"는 전제가 실제로는 "alt-screen에 있던
세션만 화면이 사라진다"로 좁혀진다.

## 고른 설계 — alt-screen을 primary로 "인쇄"해서 넘긴다

작업 지시서가 요구한 두 가지(화면 보존 + 셸 재사용성)를 **모두** primary-화면
케이스에서는 기존 메커니즘이 이미 만족하고 있었다(옛 내용이 scrollback으로 밀려나고
그 아래 fresh 프롬프트가 바로 입력 가능). 부족한 건 alt-screen 케이스뿐이었으므로,
그 경우만 다음 순서로 넓혔다(`crates/session/src/session.rs::finish_ansi_replay`):

1. 경계 리셋 **전에** 현재 alt-screen인지 확인하고(`viewport_snapshot().is_alt_screen`),
   맞으면 `backend.serialize_scrollback()`으로 그 화면을 truecolor ANSI로 뜬다 — 이미
   압축 아카이브(§14.3, agent 열람 전용 복원)가 쓰는 것과 같은 함수라 색·wide char·SGR을
   그대로 보존한다.
2. 기존과 동일하게 mode 경계 리셋 바이트를 보낸다(alt-screen 종료 → primary 복귀,
   fresh PTY가 붙을 준비).
3. 뜬 내용이 있으면 구분선 마커 + 그 내용 + 빈 줄을 **primary 화면에 다시 흘려보낸다.**
   결과적으로 primary 화면은 [이전 primary 내용] → [`-- restored screen (connection
   ended) --`] → [alt-screen이었던 내용] → [빈 줄] → [fresh 셸 출력] 순서가 된다.

이러면 화면 높이가 짧아 스크롤이 필요해도 alt-screen 내용이 **scrollback에 남아
위로 스크롤하면 보이고**, 화면 맨 아래는 항상 fresh 셸의 살아있는 프롬프트다 — 지시서가
예시로 든 "이전 내용을 스크롤백으로 복원한 위에 새 셸을 붙이는" 안을 alt-screen
케이스까지 그대로 확장한 것이다.

### 버린 대안

- **cell 단위로 직접 ANSI를 재조립**: `TerminalViewportSnapshot.visible_cells`(fg/bg/문자)를
  순회해 SGR을 새로 합성하는 방법도 가능했지만, `serialize_scrollback()`이 이미 존재하고
  압축 아카이브 왕복 테스트(`alacritty_backend.rs::scrollback_직렬화_왕복`)로 검증돼
  있어 새 코드를 만들 이유가 없었다(재사용 우선).
- **agent 아카이브(scrollback_archive) 포맷을 셸에도 쓰기**: 종료 시점에 running 셸을
  아카이브에서 제외하는 기존 결정("복원 시 respawn+로그 replay가 기대 동작이다",
  `in_process.rs` 주석)을 뒤집는 것이라 범위가 훨씬 크다 — 로그 replay 경로 자체는
  멀쩡했고 alt-screen 처리 한 곳만 고장나 있었으므로 그 지점만 고쳤다.
- **alt-screen을 아예 유지한 채 새 PTY를 그 위에 붙이기**: fresh 셸의 출력이 옛
  alt-screen(예: 죽은 vim 버퍼) 위에 그대로 겹쳐 그려져 "무엇이 지금 살아있는 화면인지"
  더 혼란스럽다 — agent 열람 전용 복원(`finish_boundary: false`)이 이 방식을 쓰는 건
  애초에 새 PTY가 붙지 않기 때문이고(주석에 명시), 셸은 PTY가 붙으므로 이 전제가
  깨진다. 채택하지 않음.
- **"연결 종료" 안내를 workspace.rs의 기존 archived-notice 배너로 표시**: agent 경로의
  배너(`render_pane`의 `archived_notice`)를 열어보니 `crate::agent_resume::
  ArchivedResumePresentation`(Exact/RecentInCwd/Unsupported/…)에 깊이 엮여 있고, 이
  파일은 다른 에이전트(codex resume) 소유다. 셸용으로 이 배너를 분기하려면 그
  소유 경계를 넘어야 해서 포기했다. 대신 "연결 종료" 사실은 **터미널 본문 안의
  구분선 마커**(`-- restored screen (connection ended) --`)로 전달한다 — 새 UI 이벤트·
  `SessionView` 필드·i18n 카탈로그 배선이 필요 없고, `restored_readonly`가 "agent만
  true"라는 기존 불변(다른 코드 여러 곳이 이를 전제)도 건드리지 않는다. 이 마커는
  로케일 카탈로그를 거치지 않는 터미널 본문 바이트라 영어로 고정했다(레포의 기본/필수
  로케일도 en-US) — **i18n 로케일 파일은 변경하지 않았다.**

## 유계 보존·redaction을 어떻게 지켰는가

- **유계**: `serialize_scrollback()`은 alt-screen 활성 중엔 `history_size()`가 0이라
  현재 화면(cols×rows, 수십 KB 수준)만 뜬다. 디스크에 새로 쓰지 않고 **이번 재시작의
  메모리 안에서만** 존재하며, 다음 재시작에도 같은 로그 파일에서 같은 절차를 다시
  거칠 뿐 파일이 누적되지 않는다. 기존 16MiB(`MAX_ANSI_REPLAY_BYTES`,
  `ANSI_LOG_MAX_BYTES`) tail 상한·`RESTORE_SCROLLBACK_LINES`(10,000줄) 등 기존 상수는
  전혀 건드리지 않았다.
- **redaction**: `serialize_scrollback()`은 **디스크나 PTY에서 새로 읽지 않는다** — 이미
  `replay_ansi`로 redacted 로그를 먹여 만들어진 **메모리상의 terminal grid**를 그대로
  ANSI로 되읽는다. 그 grid에 들어있는 내용은 애초에 `redact_chunk`를 통과해
  `redacted.ansi.log`에 쓰인 바이트뿐이므로, 이 경로로 새로 노출되는 raw secret은 없다.

## 고친 파일

- `crates/session/src/session.rs`
  - `RESTORED_ALT_SCREEN_MARKER` 상수 추가.
  - `finish_ansi_replay()`: alt-screen이면 리셋 전에 뜨고, 리셋 후 마커+뜬 내용을
    다시 흘려보내도록 확장. 비-alt-screen 경로는 바이트 하나 안 바뀜(기존 전체 스위트
    통과로 확인).

## 테스트

- **기존 테스트 갱신**: `ansi_replay_경계는_alt_screen을_끝내고_fresh_출력을_새줄에_둔다`
  (`crates/session/src/session.rs`) — 원래는 "ALT-SCREEN 텍스트가 사라짐"을 단언했는데,
  이게 바로 이번에 고친 버그의 스펙이었다. 지우거나 완화하지 않고, 좁은(40×6) 테스트
  터미널에선 보존 내용이 scrollback으로 밀려난다는 점을 반영해 `screen_text()`(현재
  화면만) 대신 `search_scrollback()`(scrollback 포함)으로 "ALT-SCREEN이 사라지지
  않았음"을, `screen_text()`로는 "FRESH-PROMPT가 지금 바로 보임"을 함께 확인하도록
  고쳤다. fresh 프롬프트가 이전 내용보다 뒤에 있어야 한다는 이전 테스트의 취지는
  그대로 유지된다.
- **새 테스트**: `재시작시_alt_screen이었던_셸_pane도_화면이_보존된다`
  (`crates/runtime/src/in_process.rs`) — `SpawnShell`로 `/bin/sh -c 'printf
  "\033[?1049hREMOTE-VIM-BUFFER"; exec /bin/cat'`를 띄워 SSH로 원격 TUI를 보다가
  연결이 끊긴 상태를 흉내낸 뒤, 워커를 재시작(`RestoreWorkspace`)하고 (1)
  `SearchScrollback` 명령으로 "REMOTE-VIM-BUFFER"가 scrollback에서 검색되는지(화면
  보존), (2) `WriteInput`을 보내 즉시 에코가 돌아오는지(셸 재사용성) 둘 다
  end-to-end로 검증한다.

## 게이트 결과

- `cargo test -p session --lib`: 51 passed, 0 failed
- `cargo test -p runtime --lib`: 275 passed, 0 failed (신규 테스트 포함)
- `cargo test -p storage --lib`: 308 passed, 0 failed (이 crate는 코드를 고치지 않았고
  회귀 확인용으로 돌렸다)
- 나머지 게이트(`cargo test -p deppy-sijo`, clippy, check-boundary, fmt)는 커밋 직전
  본문 마지막에 기록.

## 화면으로 검증 못 한 항목

- 저장소 규칙상 이 작업에서는 앱을 빌드해 실행하지 않았다(사용자 앱이 떠 있고, 재시작 시
  실제 SSH 세션이 날아갈 위험 — 화면 확인은 오케스트레이터가 사용자 동의를 받고 진행).
  따라서 실제 macOS 앱에서 SSH + vim/tmux 화면이 눈으로 보존되는지는 **자동화 테스트로만**
  검증했고 육안 확인은 하지 못했다.
- ghostty 백엔드(`ghostty-backend` feature, 기본 비활성)는 `serialize_scrollback()`을
  구현하지 않아(`None` 반환) 이 백엔드로 빌드하면 alt-screen 보존이 적용되지 않고
  기존 동작(버려짐)으로 폴백한다 — 기본 빌드(Alacritty 백엔드)에서만 검증했다.
- "연결이 끊겼다"는 사실을 터미널 본문 마커로만 전달하기로 했다(위 "버린 대안" 참고) —
  workspace.rs에 별도 배너 UI를 추가하지 않았으므로, 그 형태의 시각적 확인(egui 배너
  스크린샷 등)은 애초에 대상이 아니다.
