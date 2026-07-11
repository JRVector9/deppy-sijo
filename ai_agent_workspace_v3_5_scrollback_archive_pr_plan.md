# deppy-sijo A트랙 PR 계획 v2 — 종료 세션 스크롤백 아카이브 영속화

작성일: 2026-07-11 (Fable 5 서브에이전트 — 코드 조사 + 업계 선례 웹 리서치 기반)
대상: main(a2ca8ce) — 인메모리 압축 아카이브(§14.3 확장) 구현 완료 상태
문서 성격: Build PR 계획서 (코드 변경 없음)

---

## 0. 검토 과제 1 결론 — 분리 유지, 폴백만 통일

**결론: 별도 아카이브 파일(`scrollback.zlib`)을 유지한다. 단, `redacted.ansi.log` tail 재생을 "아카이브가 없을 때의 열람 전용 폴백"으로 흡수해, 두 메커니즘의 역할을 다음처럼 고정한다.**

| 파일 | 역할 | 성격 |
|---|---|---|
| `redacted.ansi.log` | 감사/포렌식 원천 + **살아있는 셸** 재시작 복원(respawn 후 이력 재주입) + 아카이브 부재 시 열람 전용 폴백 | 무한 append 스트림 (리셋/altscreen 잡음 포함) |
| `scrollback.zlib` (신규) | **종료 세션**의 최종 grid 스냅샷 — 인메모리 아카이브의 디스크 연장 | 유계 스냅샷 (scrollback_limit × cols로 상한) |

### 0.1 replay_saved_ansi의 실제 동작 범위 (코드 확인 결과)

- **전체 재생도, offset 이후 재생도 아니다.** 파일 끝에서 최대 16MB(`MAX_ANSI_REPLAY_BYTES`, `crates/runtime/src/in_process.rs:30`) tail을 다음 개행 경계에 정렬해 재생한다(`seek_ansi_replay_tail`, in_process.rs:2153). 개행 없는 병적 giant line이면 복원을 포기한다.
- DB의 `sessions.last_log_offset`은 기록만 되고(`persistence.rs:234`) 복원 경로에서 **소비되지 않는다** (crash-recovery 텔레메트리 성격).
- 재생 시작 시 SGR reset을 선주입해 cutoff 이전 색 상태 소실을 보정하고(`session.rs:314`), 재생 끝에 `finish_ansi_replay`(session.rs:341)가 altscreen/마우스 트래킹/scroll region/bracketed paste 모드 경계를 강제한다.
- 대상은 항상 **fresh 셸 PTY**다: `restore_pane`(in_process.rs:1397)은 모든 pane을 `SessionKind::Shell`로 respawn하며, agent 재실행 금지는 persistence.rs 헤더 주석의 안전 요구사항이다.

### 0.2 통일(로그 tail 재생 단일화)하지 않는 근거

| 축 | 로그 tail 재생 (스트림) | 아카이브 (grid 스냅샷) |
|---|---|---|
| 소스 유계성 | 무한 스트림. agent TUI(claude/codex)는 redraw 폭주로 수십~수백 MB — A트랙의 주 타깃이 최악 케이스 | grid 크기에 비례. exited cache budget(§14.3)이 이미 스크롤백을 트림한 뒤라 이중 유계 |
| 내용 충실도 | 16MB tail = 최종 화면 이전은 TUI redraw 프레임 잡음. altscreen 이력은 모드 리셋과 함께 소실. 16MB 이전 실제 콘텐츠는 영구 유실 | 최종 grid 그대로(색·wide char·wrap reflow 보존, `serialize_scrollback`이 이미 보장) |
| 복원 비용 | pane당 최대 16MB ANSI 파싱을 워커 시작 경로에서 수행 — pane 수에 비례해 워크스페이스 열기 지연 | inflate + 유계 dump feed (수백 KB~수 MB 수준) |
| 일관성 | 런 중 inflate는 정확한 grid, 재시작은 tail 재생 — 같은 pane이 시점에 따라 다른 내용 | 런 중/재시작 동일 결과 |
| 구현 비용 | 신규 영속 표면 없음 | 포맷·GC·원자성·redaction 각 1회 구현 필요 |

- **업계가 같은 갈림길에서 스냅샷을 택했다.** VS Code는 v1.60(2021)에서 "터미널 이벤트 기록 재생" 방식을 버리고 headless 터미널의 grid 상태를 직렬화하는 방식으로 마이그레이션했고, 사유로 5~10배 성능과 **손상(corruption) 위험 제거**를 명시했다 (https://code.visualstudio.com/updates/v1_60). 죽은 세션 복원 = 스냅샷(VS Code process revive, tmux-resurrect `capture-pane -e` 덤프), 살아있는 세션 재접속 = 스트림 재생(VS Code process reconnection)이라는 이분법(https://code.visualstudio.com/docs/terminal/advanced)은 우리의 "셸 respawn+replay / exited 아카이브 복원" 분리와 정확히 대응한다.
- 통일안이 성립하려면 로그에 상한·회전을 도입해야 하는데, 로그는 감사 원천이라 회전 정책 변경이 별도 제품 결정을 요구한다. 스냅샷 분리가 오히려 변경 반경이 작다.
- 단, **폴백은 통일한다**: A1 이전에 종료된 레거시 세션·손상·GC로 아카이브가 없으면 로그 tail을 열람 전용 세션(PTY 없음)에 재생한다. "아카이브 필수"가 아니라 "아카이브 우선"이 되어 마이그레이션 문제가 사라진다.

---

## 1. 판단 근거가 된 코드 사실 + 초안 전제 정정

검증된 사실 (초안과 일치):
- 인메모리 아카이브: `archive_over_cap`(in_process.rs:1944) — exited 개수 cap 64(설정으로 4~512 clamp) + 캐시 예산 초과분을 `serialize_scrollback`→zlib→`ArchivedScrollback{kind, cols, rows, scrollback_lines, exit_code, compressed}`로 강등, 총 16MB LRU. pane 재노출 시 `inflate_archived`(:2049)→`Session::restore_archived`(session.rs:140, pty 없음·열람 전용).
- `serialize_scrollback`(alacritty_backend.rs:377): history+화면 전체를 truecolor SGR로, **색 변화 시에만 SGR 방출**(xterm.js SerializeAddon과 동일한 diff 방출 — 별도 최적화 불필요), wrapped 행은 개행 없이 이어붙여 reflow 자연.
- suspend = 워커 shutdown: `suspend_warm_workspace`(app.rs:2161) → 워커 종료 블록(in_process.rs:605~627)에서 `final_drain` → 전 세션 `session_exited`(**running도 exited로 마감됨**) → `close_session_log("app-shutdown")`. A1 flush 훅의 자연 위치.

**정정 1 — "워크스페이스 삭제 시 logs/<ws>/ 정리 공짜"는 현재 사실이 아니다.** `Db::delete_workspace`(crates/storage/src/db.rs:1051)는 DB 행만 트랜잭션 삭제하고, `logs_base/<ws>/` 디렉토리는 어디서도 지우지 않는다(전 코드베이스 grep으로 확인). 아카이브를 그 아래 둬도 삭제 시 자동 정리는 없고, 기존 로그 3종과 동일하게 orphan으로 남는다. 배치 위치 자체는 여전히 옳다(같은 수명 정책 공유) — 정리는 선택 항목 A3a로 분리.

**정정 2(강화) — 아카이브 dump는 redaction 미통과 원문이다.** 로그 경로는 raw 출력을 `StreamRedactor`로 마스킹 후 기록하지만, `make_archive_entry`(in_process.rs:2012)의 dump는 grid 원문이다. "RedactionService로 마스킹 후 저장"이 필수. 좋은 소식: `redact_buffer`(crates/secret/src/redaction.rs:163)는 **ANSI escape를 제거한 텍스트에서 매칭하고 원본 범위(escape 포함)를 치환**하므로, serialize가 secret 중간에 SGR을 끼워 넣어도 마스킹이 우회되지 않는다.

**함정 발견 (A2 구현 필수 사항)** — `save_layout`은 pane의 영속 session_id를 `pipe.rows`에서 찾아 기록한다(persistence.rs:286~291). 열람 전용 복원 세션을 `rows`에 재결속하지 않으면 다음 layout 저장 때 `pane.session_id = None`이 저장되어 **그 다음 재시작부터 내용을 영구히 잃는다**. 또한 기존 `session_restored`(persistence.rs:149~176)는 row의 kind를 "shell"로, status를 running으로 **덮어쓰므로** 재사용 불가 — 전용 재결속 메서드가 필요하다.

---

## 2. 업계 선례 요약 (웹 리서치, 출처 포함)

| 선례 | 죽은 세션 내용 저장 방식 | 저장 시점 | 상한 정책 |
|---|---|---|---|
| VS Code persistent sessions | headless 터미널 grid를 ANSI로 직렬화(v1.60에서 이벤트 재생 방식 폐기) | 창 닫힘/종료 시(revive는 기본 `onExit`) | `persistentSessionScrollback` 기본 **100줄** |
| xterm.js SerializeAddon | 버퍼→ANSI escape 문자열, SGR diff 방출, 모드/alt-buffer 포함 옵션 | 호출 시 | `scrollback` 옵션(기본: 전체) |
| tmux-resurrect | `capture-pane -epJ -S -<history>` — ANSI 포함 스크롤백 전체 스냅샷(옵트인) | 수동 키(이벤트) | 압축은 미구현 TODO(#81) |
| tmux-continuum | resurrect에 위임 | **주기 15분** + 서버 시작 시 자동 복원 | — |
| iTerm2 | 장수 서버 프로세스 + macOS window restoration에 위임 | 시스템 관리 | 복원 토글(Advanced) |
| WezTerm(+resurrect.wezterm) | mux 서버는 프로세스 생존 기반(재부팅 소실); 플러그인은 JSON에 pane 텍스트 저장 | 이벤트 + `periodic_save()` | `set_max_nlines`(예: 5000줄), **원격 도메인 pane 텍스트는 저장 안 함**, age/GnuPG 암호화 옵션 |
| kitty | 세션 파일 = 구조(레이아웃/명령)만. 스크롤백 영속화(#2454)는 코어 범위 밖으로 종결 | — | — |
| asciinema v2/ttyrec | 타임스탬프 이벤트 스트림 — **재생·스트리밍 목적** | 연속 | 없음(무한 성장) |

출처: https://code.visualstudio.com/updates/v1_60 · https://code.visualstudio.com/docs/terminal/advanced · https://github.com/microsoft/vscode/issues/133516 · https://code.visualstudio.com/updates/v1_69 · https://github.com/xtermjs/xterm.js/blob/master/addons/addon-serialize/src/SerializeAddon.ts · https://github.com/tmux-plugins/tmux-resurrect/blob/master/scripts/save.sh · https://github.com/tmux-plugins/tmux-resurrect/issues/81 · https://github.com/tmux-plugins/tmux-continuum · https://iterm2.com/documentation-restoration.html · https://github.com/MLFlexer/resurrect.wezterm · https://sw.kovidgoyal.net/kitty/sessions/ · https://github.com/kovidgoyal/kitty/issues/2454 · https://docs.asciinema.org/manual/asciicast/v2/

**초안에 없던 차용 아이디어**
1. **빈 스냅샷 억제** — 실질 내용 없는 grid는 기록 생략 (VS Code v1.69 노이즈 억제)
2. **altscreen 정책의 명시적 결정** — VS Code revive는 altscreen 의도적 미복원. 우리는 반대로 "종료 순간 보이던 화면 그대로" 채택, 문서/테스트로 고정
3. **콘텐츠 영속 opt-out 선례** — resurrect.wezterm 원격 pane 미저장/암호화 옵션 → 선택 항목 A3b
4. **주기 저장은 반려** — (a) 실행 중 출력은 이미 redacted.ansi.log에 실시간 연속 기록, (b) exited grid는 불변이라 exit+suspend 이벤트 기록으로 충분. kill -9 크래시에도 로그 폴백 동작
5. **타임스탬프 포맷은 과잉** — 단, 헤더에 width/height 메타 내장 관례는 차용

---

## PR-A1 — 종료 세션 스크롤백 디스크 아카이브 (기록·읽기·GC)

### 목표
exited 세션의 압축 스크롤백 아카이브를 디스크에 영속화해, 워커 메모리 아카이브(16MB LRU)가 suspend(워커 종료)·앱 재시작·예산 축출로 소실되지 않게 한다.

### 범위
- 포함: 아카이브 파일 포맷/저장 모듈(crates/storage 신규 `scrollback_archive.rs` — logs.rs와 같은 "redacted 바이트만 수신" 계약), 기록 훅(exit·shutdown), 읽기+검증, 인메모리 miss 시 디스크 폴백, 워크스페이스별 GC.
- 제외: 재시작 복원 UX(PR-A2), 로그 회전, remote 워커 경로, 워크스페이스 삭제 시 디렉토리 정리(A3a).

### 구현 요점
1. **경로**: `logs_root/<세션 UUID>/scrollback.zlib` — `redacted.ansi.log`와 같은 디렉토리(같은 수명 정책). UUID는 `PersistPipe::session_log_key`(persistence.rs:179), 경로 검증은 기존 `session_dir_key`(logs.rs:48) 재사용. **persist 세션(UUID 보유)만 대상** — `run_logs_root` 세션(비영속)은 메모리 아카이브만 유지.
2. **포맷**: 고정 LE 헤더 = magic(`DPSA`) + version(u8=1) + kind(u8) + cols(u16) + rows(u16) + scrollback_lines(u32) + exit_code 유무(u8)+값(u32) + uncompressed_len(u32), 이어서 zlib 스트림. 복원 메타를 파일에 자급 — `terminal.size` sidecar에 의존하지 않는다. *차용: 헤더 메타 내장(asciinema v2 관례)*. 별도 체크섬 없음 — zlib adler32가 본문 무결성 검증, 헤더는 magic/version/필드 범위 검증(cols·rows 1..=500). `uncompressed_len` 상한 검증 + `Read::take`로 압축 폭탄 방어. 검증 실패·inflate 실패는 warn 후 **graceful skip**(파일 삭제).
3. **기록 시점 (이벤트 저장)**: *차용: VS Code revive `onExit`*
   - `just_exited` 처리(in_process.rs:1788~1820) 직후 1회: serialize→redact→deflate→기록. 종료 grid는 불변이므로 재기록·주기 저장 불필요.
   - 워커 shutdown 블록(in_process.rs:605~627)의 `final_drain` 이후: 미기록 exited분 + **아직 running인 agent 세션**의 최종 grid를 flush — suspend가 프로세스를 죽여 DB상 exited로 마감되므로 이것이 "suspend 직전 미기록분"의 정확한 정의. 이미 파일이 있으면 skip(불변).
   - *차용: 빈 grid(트림 후 실질 내용 없음)는 기록 생략*.
4. **Redaction**: dump를 새 `stream_redactor()`로 `redact_chunk`+`flush` 통과 후 압축(§7). storage 모듈 계약은 logs.rs와 동일 — "평문 secret을 받지 않는다". 한계(기존 로그와 동일): 이번 실행에서 register된 secret만 마스킹.
5. **원자성**: 같은 디렉토리에 tmp 기록 후 rename. tokio 금지 — 워커 스레드에서 동기 수행. exit는 저빈도라 tick 지연 허용, 측정 후 문제 시 스레드 오프로드 후속.
6. **읽기(디스크 폴백)**: `inflate_archived`에서 인메모리 miss 시 `session_log_key`로 파일을 읽어 `Session::restore_archived`. 런 중 충실도가 재시작과 동일해진다.
7. **GC**: 워크스페이스당 64MB, `logs_root/*/scrollback.zlib`만 대상(로그 3종 불가침), mtime LRU, 기록 직후 수행. 축출·orphan은 PR-A2의 로그 폴백이 받침.
8. **altscreen 정책(명시 결정)**: 종료 순간의 활성 grid를 그대로 스냅샷 — "마지막으로 보이던 화면" 보존.

### 완료 기준
- 라운드트립: serialize→redact→기록→읽기→`restore_archived` 후 `screen_text()` 일치 + register된 secret이 파일 바이트에 부재.
- 원자성: 기록 실패 시 부분 파일 미노출, tmp 잔재 정리.
- 손상 내성: 잘린 파일/magic 불일치/inflate 실패 각각 graceful skip.
- GC: 64MB 초과 시 mtime 오래된 것부터 제거, 로그 파일 불가침.
- shutdown flush: suspend 경로에서 미기록 exited + running agent의 파일 생성.
- 공통 게이트: fmt/clippy/test --workspace.

### 리스크
- 대형 grid serialize+deflate의 워커 tick 지연 — exit 저빈도로 완화, 측정 후 오프로드 여지.
- 로그와 아카이브의 개념적 중복 — §0 역할 분리로 정당화.
- visible 상태로 종료된 세션은 dump가 큼(최대 visible budget) — 유계이므로 수용, GC가 총량 방어.

---

## PR-A2 — 앱 재시작 복원: agent pane 열람 전용 복원

### 목표
agent였던 pane을 재시작(및 suspend 해제) 후 fresh 셸로 대체하지 않고, 디스크 아카이브(1차)·로그 tail(폴백)로 **열람 전용** 복원한다. 셸 pane은 현행(respawn+`replay_saved_ansi`) 유지.

### 범위
- 포함: `restore_pane`(in_process.rs:1397) 분기, PersistPipe 이전-row 조회/재결속 API, 폴백 재생, 상태 이벤트 정합.
- 제외: agent 프로세스 재실행(안전 요구사항 위반), 셸 pane 동작 변경, 새 UI 상태 추가.

### 구현 요점
1. **게이트 = 이전 row의 `session_kind == "agent"`**. status 문자열은 게이트로 쓰지 않는다 — status는 done/error/idle 등 detector 값이 남을 수 있고, suspend 시엔 전 세션이 exited로 마감되므로 "agent kind"만으로 충분·안전(agent는 어떤 경우에도 재실행하지 않으므로 running이었어도 열람 전용이 옳다 — A1의 shutdown flush가 최종 grid를 이미 기록).
2. **PersistPipe 확장**: (a) `session_restored`가 kind를 "shell"로 덮어쓰기 전에 조회할 peek API(`restored_session_kind(persistent_id)`), (b) 열람 전용 재결속 메서드 — 이전 UUID row를 `rows`에 결속하되 kind("agent")·exited status 보존(기존 `session_restored`는 status를 running으로 되돌리므로 재사용 금지). **결속 누락 금지**: 미결속 시 다음 layout 저장에서 `pane.session_id=None` → 이후 재시작부터 내용 영구 유실.
3. **복원 절차**: 디스크 아카이브 존재 → 헤더 메타로 `Session::restore_archived` → pane 연결. 부재/손상/GC → 새 backend에 로그 tail을 **열람 전용 세션으로** 재생(기존 `replay_ansi`+`finish_ansi_replay` 재사용, PTY spawn 없음) — 레거시 세션 커버. 로그마저 없으면 현행 "세션 잃은 pane" 모델. *차용: 스냅샷 1차 + 스트림 폴백 2계층(VS Code revive/reconnection 이분법)*
4. **기존 UI 상태 재사용**: `restore_archived` 상태(pty=None, Exited, cache class Exited)는 런 중 inflate와 동일 — 신규 UI 개념 없음. 복원 직후 `SessionExited`/status view 이벤트 emit으로 배지·정렬 정합 확인. StatusDetector·`open_session_log`는 설치하지 않는다(프로세스·신규 출력 없음).
5. 복원된 세션은 `sessions`+`exited_order`에 정상 편입 — 기존 cap/예산 사이클 참여(재-archive 시 파일 존재로 기록 skip).

### 완료 기준
- 게이트 단위 테스트: agent row→열람 전용(spawn 없음), shell row→현행 respawn+replay 회귀 없음.
- 아카이브 부재 agent pane→로그 폴백 재생으로 열람 전용 복원.
- **2회 왕복 테스트**: 복원→layout 재저장→재복원에서 pane↔UUID 연결 유지(§1 함정 회귀 방지).
- suspend→해제: 종료된 agent pane 내용이 워커 재생성 후에도 보임(원 문제 해소 검증).

### 리스크
- 사용자가 그 pane에서 이어서 작업하길 기대할 수 있음 — 기존 exited pane과 동일 UX로 안내 확인, 회귀 판단 시 "열람 전용 + 원클릭 새 셸" 후속.
- row 재결속 실수 시 데이터 유실 — 전용 테스트로 방어(완료 기준 3).
- suspend로 죽은 running agent의 마지막 화면이 "실행 중처럼" 보일 수 있음 — exited 배지/exit_code 표기로 완화.

---

## PR-A3 (선택 — 별도 승인 후 진행)

**A3a. 워크스페이스 삭제 시 `logs_base/<ws>/` 정리** — §1 정정 1의 후속. 현재는 로그·아카이브 모두 orphan으로 남는다(기존 동작). **감사 로그까지 파괴하는 동작 변경**이므로 제품 결정(사용자 확인) 필요. 소형 독립 PR.

**A3b. 콘텐츠 영속 opt-out 설정** — "종료 세션 내용을 디스크에 남기지 않음" 토글. 로그 자체가 이미 디스크에 남는 구조라 실익 제한적 — 요구 발생 시에만.

## 반려한 대안 (근거)
1. **로그 tail 재생으로 통일** — §0.2. 충실도·시작 비용·일관성 열세, VS Code가 동일 방식을 폐기한 선례.
2. **주기 체크포인트(tmux-continuum식)** — 로그 실시간 기록 + exited grid 불변이라 이벤트 기록으로 충분.
3. **타임스탬프 포맷(asciinema/ttyrec)** — 재생용, 최종 상태 복원에 과잉 + 무한 성장.
4. **다건 보관 + last 포인터** — 세션당 최종 상태 1건이면 충분. 과거 시점 열람은 로그의 역할.
5. **저장 암호화(age/GnuPG)** — secret은 Redaction 마스킹 후 기록이라 위협 모델상 불요.
