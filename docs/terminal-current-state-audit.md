# Terminal 현행 구현 감사 (PR-01 산출물)

작성일: 2026-07-13
기준 커밋: `b608ec4` (감사 시점 main)
방법: `docs/terminal-renderer-audit-first-pr-plan.md` §4 판정 규칙(KEEP/IMPROVE/REPLACE/UNKNOWN)에 따라
4개 도메인을 병렬 감사(Opus 4.8 ×4, 읽기 전용)하고 오케스트레이터가 근거를 재검증·통합했다.
런타임 계측(워크스페이스 1·5·10, 생성·삭제 반복 RSS)은 미실시 — 각 도메인의 "측정 방법"으로
절차를 정의했고, 이것이 PR-01의 잔여 작업이다.

## 종합 요약 — PR별 게이트 판정

| PR | 계획 | 감사 판정 | 근거 요약 |
|---|---|---|---|
| PR-02 생명주기/누수 | 수정 | **대폭 축소 → 검증 PR** | 점검 7항목 전부 기구현(명시적 shutdown/flag/Weak/bounded/kill-wait). 잔여: 100회 반복 RSS 자동화 + 삭제 시 logs 디렉터리 잔존 정리 |
| PR-03 공유 리소스 | 구조 작업 | **대폭 축소 → 검증 테스트만** | egui Context 단일 = Fonts/Atlas/텍스처 앱 전역 1개. 워크스페이스는 GPU/폰트 미소유(갤리 캐시뿐) |
| PR-04 wgpu/Metal | 전환 | **진행 (목표 필수)** | 현행 glow 결함 없음. 변경 지점 좁음(NativeOptions renderer + eframe feature). 기준선 측정 선행 |
| PR-05 전용 렌더 경로 | 교체 검토 | **측정 게이트 후 결정** | 사전판정 4조건 중 2충족(전체 재구성 없음·hidden 미렌더), 2 UNKNOWN(대량출력 p95·Retina 품질). 통과 시 epaint KEEP |
| PR-06 Glyph 품질 | glyphon 검토 | **보류 (측정 조건부)** | D2Coding 2:1 정합 + wide/폴백/글리프 치환 기구현. 시각 비교가 나쁠 때만 진행. 별도 격차: bold/italic 등 속성 렌더 필드 부재 |
| PR-07 Dirty/비활성 | 구현 | **스킵급 축소 → 계측 검증만** | 전 항목 기구현(§14 정책). cursor blink는 존재 자체가 없음(항목 무의미) |
| PR-08 Selection | 구현 | **부분 진행** | KEEP 다수. 신규: triple-click(하), mouse reporting+Shift override(중상, 순서 종속), scrollback 선택(상). 버그 #1은 본 감사에서 수정 완료 |
| PR-09 Clipboard | 교체 검토 | **IMPROVE 5건으로 축소** | 전 구간 백그라운드 기구현(REPLACE 미해당). 취소 토큰/중복 해시/원자 write/종료 정리/계측만 |
| PR-10 생성 버스트 | 최적화 | **축소** | cap/LRU/3계층 아카이브/유계 replay 기구현. 잔여: 계측 + replay 16MB↔10k줄 불일치 검토 + lazy load 요구 재확인 |
| PR-11 테스트 자동화 | 구축 | **진행** | 계측 격차(아래)가 곧 이 PR의 입력 |
| PR-12 기본 경로 전환 | 정리 | PR-04/05 결과 종속 | — |

## 감사 중 발견 → 즉시 수정된 항목 (본 커밋 포함)

1. **[버그, 높음] 드래그 오토스크롤 자기 중단**: `send(Scroll)`의 선택 해제(휠 UX)와 충돌해
   첫 정수 행 스크롤 직후 선택·오토스크롤이 함께 죽고, 앵커 보정 경로가 도달 불가였다.
   → `send_keep_selection` 분리로 수정(workspace.rs). 휠/타이핑의 해제 정책은 불변.
2. **[stale 주석] alacritty_backend.rs** "dirty_ranges 소비자 없음" — 실제는 Session이 채우고
   renderer_egui가 소비. 교정.
3. **[stale 주석] viewport_snapshot.rs** "현재 렌더러는 전체를 그린다" — 렌더러는 dirty 행
   갤리 캐시를 사용. 교정.

## PR-01 잔여 작업 (계측 격차)

- 단계별 RSS 로그(시작 단계·워크스페이스 생성 전후), 워크스페이스 생성 시간·첫 화면 시간 측정 — 부재
- repaint 원인(RepaintCause) 캡처를 FrameStats에 추가 — 부재
- 이미지 붙여넣기 구간별 시간 계측(clipboard read/decode/encode/write) + macOS 빠른경로 hit율 — 부재
- 생성·삭제(전환·suspend) 100회 RSS 자동화 하네스 — 부재 (절차는 §생명주기 "측정 방법" 정의)
- glow 기준선 CPU/RSS (PR-04 비교용) — 부재
- steady-state 렌더 루프 heap allocation 계측 (§3-B 검증) — 부재
- 기존 계측 자산: `resource_monitor`(앱/자식 트리 RSS 분리, 이미 §3 목표 충족),
  `perf.rs` FrameStats(p95)+부하 하네스, `rebuilt_rows_last_frame`(테스트 전용)

---

## 렌더링·GPU·폰트

### 영역별 판정표

| 항목 | 현재 구현 (파일:라인 근거) | 판정 | 근거 요약 |
|---|---|---|---|
| eframe renderer 백엔드 | `main.rs:59-85` `NativeOptions{..Default}` → `Renderer::Glow`. `Cargo.toml:33` eframe 기본 feature | KEEP (PR-04 변경 지점) | 현재 **glow/OpenGL** 확정, 결함 없음. wgpu는 lock에만 존재·미컴파일 |
| 터미널 셀→egui 전달 경로 | pump→`session.rs:294-299` take_snapshot→Viewport 이벤트→`workspace.rs` SessionView.snapshot→`renderer_egui::draw(snapshot, render_cache)` | KEEP | 세션별 Arc 스냅샷 단방향 전달, 색은 backend에서 RGB 해석 완료 |
| 매 프레임 전체 텍스트 재생성 (REPLACE 조건 #1) | `renderer_egui.rs:38-83` 세션별 행 갤리 캐시(retain), `150-183` dirty 행만 `build_row_cache`, 테스트 `650-663`("변화 0→재구성 0, 1행 dirty→1행") | **KEEP (조건 미해당)** | 갤리 **shaping은 dirty 행만**. 단 shape 발행/테셀레이션은 매 repaint 발생(아래 nuance) |
| 더티 파이프라인 | `alacritty_backend.rs:195-205` term.damage→dirty_rows, `session.rs:215-223,294-299,434-443` 누적·스냅샷 반영·소거, `renderer_egui.rs:393-408` 소비 | KEEP | 엔드투엔드 dirty 추적 실동작 |
| 폰트/텍스처 생성·공유 | `fonts.rs:85-148` 번들 D2Coding/JetBrains Mono + CJK 폴백, 시작 1회 + 설정 hot reload. 단일 `egui::Context` | KEEP | **Fonts/Atlas/텍스처 매니저 앱 전역 1개.** 워크스페이스는 미소유 |
| egui 텍스처 | QR 1개(URL당 캐시, 설정 닫힘 해제). 클립보드 이미지는 텍스처 미생성 | KEEP | 수명 관리됨 |
| 활성/비활성 렌더 게이트 | active만 `.show()`, 창 가시성→SetWorkspaceState(Warm), worker `render_active` 게이트 | KEEP | §14 정책 기구현 |
| 프레임 계측 | `perf.rs` FrameStats(5s 윈도, p95), `DEPPY_FRAME_STATS` 게이트 | IMPROVE | repaint 원인·자원 생성 횟수 미기록 |
| idle repaint | 전 `request_repaint`가 조건부. blink 부재(grep 0) | KEEP | 이벤트 드리븐, idle-0 |

**nuance (REPLACE 조건 #1 정밀 해석):** 갤리 *레이아웃*은 dirty 행만 재생성하지만, repaint가
발생한 프레임에서는 모든 행의 shape 발행+테셀레이션이 다시 일어난다(egui immediate-mode).
"재-shaping 회피"는 달성, "shape 수집 비용"이 PR-05의 실제 최적화 여지다.

### PR-03/04/05/06/07 사전판정 권고

- **PR-03: 대폭 축소.** 단일 Context 구조로 이미 충족 — "프로세스당 1개" 검증 테스트만 추가.
- **PR-04: 진행(목표 필수).** glow 결함 없음 단서 유지. 변경 지점: `main.rs` NativeOptions
  `renderer: Wgpu` + eframe feature(+glow fallback flag). glow 기준선 측정 선행.
- **PR-05: 측정 게이트.** 사전판정 4조건 중 ①전체 재구성 없음 ②hidden 미렌더 = 충족,
  ③대량출력 프레임 시간 ④Retina 품질 = UNKNOWN. ③④ 통과 시 epaint KEEP 권고.
- **PR-06: 보류.** D2Coding 정합/wide/폴백/치환 기구현 — 시각 비교 결과가 나쁠 때만.
  별도 격차: `TerminalCell`에 bold/italic/underline 속성 필드 자체가 없음(기능 격차로 기록).
- **PR-07: 스킵급 축소.** 전 항목 기구현. cursor blink는 미구현이라 "분리" 항목 무의미(idle-0에 유리).

### UNKNOWN 측정 방법

- 대량출력 p95: `DEPPY_FRAME_STATS=1 DEPPY_PERF_HARNESS=1` 실행 + 활성 탭 대량 출력 변형, 임계 p95≤16ms.
- Retina 품질: 1x/2x 한글+ASCII 캡처, Ghostty/Termius 비교(수동).
- zero-allocation: 카운팅 GlobalAlloc 또는 Instruments. 알려진 alloc: 스냅샷 `vec![cells]`(워커 스레드),
  dirty 행 rebuild 시 Vec/String(유계).
- glow vs wgpu: 동일 하네스 2회 실행 + RSS/CPU 샘플링.

---

## 생명주기·메모리

### 앱 시작 경로

| 항목 | 현재 구현 (파일:라인) | 판정 | 근거 요약 |
|---|---|---|---|
| 시작 순서 | `main.rs:21-111` paths→logging→LockFile→config→keyring→SQLite→orphan reconcile→eframe | KEEP | 창 생성 전 무거운 작업 없음 |
| 시작 생성물 | `app.rs:1208-1236` 워커 1개만. remote/web은 None, 하네스는 env 게이트 | KEEP | 과잉 초기화 없음 |
| 복원 | background dotenv 적용 후 워커가 RestoreWorkspace 처리 | KEEP | UI 첫 프레임 미블록 |

### Workspace 생성·삭제·전환 (PR-02 핵심)

| 항목 | 현재 구현 (파일:라인) | 판정 | 근거 요약 |
|---|---|---|---|
| 생성 | `app.rs:1443-1501` make_runtime → 워커 스레드 1개 | KEEP | GPU/폰트 자원 미생성 |
| 전환 | `app.rs:2442-2557` warm 풀 재사용 + live warm hard cap 사전 거부 | KEEP | 누적 원천 차단 |
| warm 상한/축출 | evict_warm/evict_idle_warm(30분) | KEEP | live 보호는 의도된 정책 |
| suspend | `app.rs:2698-2742` drain→취소 방어→background shutdown→pending_shutdowns 추적 | KEEP | 이중 워커 경합 차단 |
| 삭제 | `app.rs:5101-5119` warm shutdown + DB 행 삭제 | **IMPROVE** | **`logs/<ws_id>/` 디스크 미정리**(로그·zlib 잔존). shutdown이 UI 동기(경미) |
| shutdown | `in_process.rs:250-272` flag+채널 drop+unpark+join, Drop 폴백 | KEEP | P5 데드락 수정 반영 |
| 앱 종료 | on_exit: web→remote→active→warm 순 + pending join | KEEP | — |

### PR-02 점검 목록별

| 항목 | 근거 | 판정 |
|---|---|---|
| PTY reader 종료 | bounded sync_channel(64), Drop이 recv drop+EOF로 해제. join은 의도적 미실시(grandchild slave 블록 방지, 문서화) | KEEP |
| child kill/wait | SIGHUP(pgid)→bounded reap→SIGTERM→200ms→SIGKILL, try_wait reap, EXIT_WAIT_TICK_CAP | KEEP |
| channel/Arc 순환 | fan-out strong_count/send 실패 정리, dashboard wake는 Weak, 고빈도 이벤트는 최신값 slot | KEEP |
| callback/timer 잔존 | 워커 park 루프뿐(join 소멸), 감지 epoch 폐기, pending_shutdowns 추적 | KEEP |
| egui texture 해제 | 터미널 텍스처 미생성(갤리 캐시는 MuxUpdated retain 정리), QR은 drop 해제 | KEEP |
| 로그 버퍼 해제 | close_session_log가 모든 경로(kill/close/exit/shutdown)에서 호출 | KEEP |

### Term/스크롤백 (PR-10 근거)

| 항목 | 근거 | 판정 |
|---|---|---|
| 초기 용량 | `scrolling_history`는 상한(선할당 아님) + byte 예산 유도 | KEEP — 계획의 "대형 선할당" 우려 미해당 |
| 클래스별 cap | VISIBLE 10k/16MB, HIDDEN·EXITED 1k/2MB, 전이 trim(테스트 존재) | KEEP |
| exited LRU + 예산 | cap 64(4..512 clamp) + 전역 128MB, 초과분 zlib 강등 | KEEP |
| 아카이브 | 메모리 16MB LRU + 디스크 64MB gc + 증분 예산 캐시 + lazy inflate | KEEP — 3계층 유계 |
| 이중 보관 | Term=메모리, 로그=디스크 redacted — 메모리 이중 보관 없음 | KEEP |

### 세션 로그 복원

- replay는 **16MB tail + 64KB chunk 스트리밍**(전체 적재 아님) — KEEP.
- IMPROVE(경미): 16MB tail vs 스크롤백 10k줄 예산 불일치 — 상한 하향 시 재시작 burst CPU 절감.
- UNKNOWN: "과거 로그 lazy load"(16MB 이전 앱 내 열람 불가)는 실제 요구 재확인 후 결정.

### RSS 계측

- **앱/자식 분리 측정은 기구현**(`resource_monitor.rs` — pgid∪자손 트리, 2s+변화 게이트).
- 부재: 단계별 RSS 로그, 생성 시간·첫 화면 시간 측정 → PR-01 잔여.

### PR-02/10 권고 및 측정 방법

- **PR-02: 대폭 축소** — 본체는 "생성·삭제 100회 RSS 자동화". 경미 IMPROVE 2건(logs 디렉터리 정리, 삭제 경로 동기 shutdown 관찰).
- **PR-10: 축소** — 계측 선행 필수. replay 상한 검토, lazy load 요구 확인. "renderer lazy init/선할당 분할/단계적 시작"은 현 구조상 해당 없음 → 스킵.
- 측정: ① headless 하네스(#[ignore] 테스트/xtask)로 생성→spawn→shutdown 100회 + RSS 선형회귀 기울기≈0 판정 + 잔존 프로세스/스레드 0 확인. ② GUI 전환 반복 + 2s RSS 샘플링. ③ >16MB 로그 재시작 burst 측정.

---

## 입력·마우스 선택 (PR-08 사전판정)

| # | 기능 | 근거 (파일:라인) | 판정 | 비고 |
|---|---|---|---|---|
| 1 | pointer→cell 변환 | `workspace.rs:1261-1271` cell_at, origin/cell_size는 renderer와 동일 소스, UI배율 역보정, 좌우 패딩 테스트 | KEEP | Retina는 egui 논리 좌표로 투명, 렌더-선택 격자 일치 |
| 2 | selection lifecycle | 시작 1287-1303 / 확장 1304-1332 / 해제 1360-1362·send·hidden | KEEP | 앵커-끝점 모델 |
| 3 | 드래그 + edge auto-scroll | rate/앵커보정 + 단위테스트 | ~~IMPROVE~~ → **수정 완료** | 버그 #1(자기 중단)을 본 감사에서 수정 — send_keep_selection 분리 |
| 4 | double / triple click | double 기구현(1272-1286, +URL 열기) / triple 부재 | double KEEP / **triple 미구현** | egui triple_clicked 미사용 |
| 5 | Shift override + mouse reporting | mouse reporting 자체 미구현(input_mapper.rs:3 "PR-21까지" 백로그) | **미구현** | 앱이 마우스를 PTY로 리포트하지 않아 현재 모든 드래그가 이미 로컬 선택 — override는 reporting 선구현이 전제 |
| 6 | 화면 밖 capture | Sense=click_and_drag + interact_pointer_pos + clamp | KEEP | — |
| 7 | scrollback 좌표 반영 | freeze(576-588)+드래그 중 앵커보정. 화면 밖 선택은 표현 불가(설계 clamp) | IMPROVE | 버그 #1 수정으로 보정 경로 도달 가능해짐. 완전한 해법은 scrollback-절대 좌표 선택 모델(난이도 상) |
| 8 | highlight + 추출(Cmd+C/wide) | renderer_egui 410-516, CJK/emoji 테스트 통과 | KEEP | 선택 있을 때 Cmd+C가 ^C보다 우선 |
| 9 | 해제 정책 | 클릭/입력·스크롤 send/hidden/테마. 출력 도착은 해제 아닌 freeze | KEEP | 표준 동작 |
| 10 | 검색(Cmd+F)과 상호작용 | text_edit_focused 게이트 격리, 레이어링만 겹침 | KEEP | 하드 충돌 없음 |

### PR-08 권고 (남은 작업 구체화)

1. triple-click 줄 선택 — 난이도 하.
2. 터미널 mouse reporting(SGR 1006 등) PTY 전달 — 난이도 중상 (backend 스냅샷에 mouse mode 노출 + 인코딩 + 라우팅).
3. Shift override — 2 완료 후 분기만(중). 2 없이는 착수 불가.
4. scrollback 선택(화면 밖) — 선택 모델 리팩터(상). 검색의 line_from_bottom 좌표와 통일 가능.
- (완료) 버그 #1 수정 — 오토스크롤 send가 선택을 해제하지 않도록 분리.

### 잔여 위험 (보고)

- residual 필드(scroll/drag_autoscroll)가 pane 간 공유 단일 필드 — 현재 무해, 동시 드래그 도입 시 재검토.
- cell_at clamp로 우측 여백 클릭이 마지막 열에 매핑 — 실무상 무해.

---

## 클립보드 이미지 파이프라인 (PR-09 사전판정)

### 구간별

| 구간 | 스레드 | 근거 (파일:라인) | 판정 | 비고 |
|---|---|---|---|---|
| ⌘V 감지·job 등록 | UI | workspace.rs:2320-2338, 1551-1561 | KEEP | 프레임당 bool 1회 |
| clipboard read | 백그라운드 | clipboard_image.rs:6,21,55,68-77,103 | KEEP | arboard 전부 백그라운드 |
| 파일 URL 우선 | 백그라운드 | :7-9,54-62 — 파일 있으면 디코딩 없이 반환 | KEEP | §3 목표 ② 충족 |
| decode/copy | 백그라운드 | macOS 변환0(NSData→Vec 1회), fallback RGBA 차용 | KEEP | Base64 전무 |
| encode(PNG) | 백그라운드 | macOS 무인코딩 빠른경로 / fallback PngEncoder Fast | KEEP | UI 미블록 |
| temp write | 백그라운드 | fs::write(비원자) / 경로 pid+millis+uuid | **IMPROVE** | tmp+rename 아님(모바일 P6d는 원자적 — 비대칭) |
| dispatch | UI | poll_paste_task → 경로 문자열 bracketed paste WriteInput | KEEP | 이미지 바이트/base64 미전달 |
| text fallback | **UI** | read_clipboard_text (workspace.rs:2030) | IMPROVE | UI 스레드 동기 clipboard 접근(텍스트라 경미) |

### §3 목표 충족

① UI 스레드 미수행 = 충족(텍스트 fallback 단서) / ② 파일 URL 우선 = 충족 /
③ 중복 복사 제거 = 대체로 충족(Base64 전무) / ④ 취소·중복 제어 = **부분**(소비자측 폐기만,
생산자 취소·중복 해시 없음, TTL 10s로 stale 오삽입은 방지).

### 임시파일·오류

- `cache_dir/clipboard-images/`, 32MB(PNG)/40MP 상한, 24h TTL prune(paste 시 best-effort).
- 워크스페이스/앱 종료 시 명시 정리 없음(TTL 의존). 스크린샷 레이스 150ms×4 재시도.
- web-remote 업로드(P6d)와 코드·디렉터리 공유 0 — 완전 독립(모바일이 원자 기록으로 더 강함).

### PR-09 권고

- REPLACE 미해당 — 아키텍처 유지. IMPROVE 5건: ① 취소 토큰(연타 고아 작업) ② 중복 이미지 해시
  ③ 데스크톱 temp 원자 write(모바일과 정합) ④ 종료 시 정리 ⑤ 구간별 시간 계측(+빠른경로 hit율).
- 병목 후보(계측 필요): fallback PNG 인코딩(수백 ms 이력 주석), arboard get_image 디코드 —
  macOS 빠른경로가 회피하므로 hit율 계측이 우선.

---

## 계획 문서 ↔ 코드 불일치 통합 목록 (계획 개정 권고)

1. PR-02 "구현" 후보 전부 기존재 → 검증 PR로 재기술.
2. §2/PR-10 "스크롤백 대형 선할당" 우려 — 실제는 상한만, 선할당 없음.
3. §3 "앱/자식 RSS 분리 측정" — 기구현(resource_monitor). PR-01은 검증·보강 대상.
4. PR-10 "전체 로그 복원" 우려 — 16MB tail + chunk 스트리밍 기구현.
5. §7 "종료 시 thread 잔존 없음" — PTY reader detach는 문서화된 유계 예외로 완료 기준 정정 필요.
6. §3-B "workspace GPU 객체 누적" 검증 — 현 epaint 경로는 workspace별 GPU 객체 자체가 없어 PR-04/05 이후에만 유효.
7. §4 REPLACE 조건 #1("매 프레임 전체 재생성") — 현 코드 미해당(행 갤리 캐시). PR-05 근거를 "shape 발행/테셀레이션 비용"으로 재기술.
8. PR-07 "cursor blink 분리" — blink 미구현으로 항목 무의미.
9. 미기재 격차 추가 권고: workspace 삭제 시 `logs/<ws_id>/` 디스크 정리 정책, `TerminalCell` 텍스트 속성(bold/italic/underline) 필드 부재.
