# v3.6 — 모바일 자유 입력 + 연계 워크플로 PR 계획

목표: 폰에서 세션에 **자유롭게 입력**(한글 IME 포함)하고, 에이전트 상호작용(질문 답변,
메뉴 선택, 새 지시, 이미지 첨부)을 **폰에서 완결**한다. "데스크톱을 열지 않고 폰만으로
에이전트와 한 턴을 주고받는다"가 완료 그림이다.

표기: 본 문서의 PR-P6*는 v3.3 P트랙(P1~P5)의 후속 번호다.

## 0. 전제와 설계 결정 (v3.3 P5 + deppy-mux 비교에서 확정)

### 0.1 입력 모델 — composer(라인 버퍼) 우선, raw 키보드는 후순위

모바일 키보드+IME는 키 단위 스트리밍과 상극이다(조합 중간 상태가 PTY로 새면 한글이
깨진다). deppy-mux와 동일하게 **composer(textarea) + 전송 버튼**을 1차 모델로 한다:

- textarea는 브라우저가 IME 조합을 네이티브로 처리 — 완성된 텍스트만 전송된다.
- 에이전트 워크플로(claude/codex 프롬프트)는 라인 지향이라 composer와 정합.
- TUI(vim 등) 대응 raw 키 모드는 P6e(미결정)로 이연 — 시청 중심 사용에서 니즈 확인 후.
- 화살표/Esc/Tab 등 **특수키는 composer를 우회하는 키 행(row)** 으로 즉시 전송 —
  claude code의 메뉴 선택(화살표+Enter)이 폰에서 되는 것이 핵심 사용례다.

### 0.2 입력 행동 계약 (deppy-mux plans/connect-pwa-web.md에서 차용 — 실전 버그 증류)

1. **보이는 터미널 surface가 target을 소유한다** — 입력은 항상 "이 접속이 시청 중인
   세션"에만 간다(P5d Key 게이트와 동일 원칙, 서버 강제).
2. **composer는 전송 시점에 target을 캡처한다** — 입력 도중 세션 전환이 일어나도
   이미 캡처된 세션으로만 간다(잘못된 터미널로 입력 가는 버그의 근본 차단).
3. **전송 실패 시 draft 보존** — WS 단절/backpressure 거부 시 composer를 비우지
   않는다. 성공 확인(낙관적: send 성공 + 에러 프레임 부재) 후에만 클리어.
4. **raw 텍스트/여러 줄 붙여넣기/이미지 첨부는 별도 경로** — 의미가 다르므로 메시지를
   분리한다(text 전송 / bracketed paste wrap / 업로드 후 경로 삽입).

### 0.3 보안 경계 (기존 모델 유지 — 확장 아님)

페어링 토큰은 이미 "셸 접근과 동등"(settings.token_sensitive_warning)으로 문서화돼
있어 자유 입력이 위협 모델을 넓히지 않는다. 다만 방어층은 유지·강화한다:

- **클라이언트發 이스케이프 주입 차단**: Input 텍스트에서 C0 제어문자(\t 제외)를
  strip — 제어 시퀀스는 서버 화이트리스트 키(named key)로만 생성된다.
- 입력도 시청 게이트(watch.watched == session) 뒤에서만.
- 프레임 상한 분리: 제어 메시지 64KB 유지, Input/paste는 별도 상한 256KB
  (ws_api.rs:32 주석의 예약 이행). PTY 큐 backpressure는 이벤트로 폰에 중계.
- 선행 커밋: **Referrer-Policy + Permissions-Policy 공통 헤더, WS 업그레이드
  Origin 화이트리스트**(mux 차용, 방법 B 대비 심층 방어 — 코드 부재 확인됨).

### 0.4 재사용 자산 (코드 확인 완료)

```text
- RuntimeCommand::WriteInput — 그대로 재사용(P5d send_key와 동일 싱크 경로).
- RuntimeEvent::Viewport.bracketed_paste — 브리지가 시청 세션별 최신값을 캐시해
  여러 줄 paste를 서버측에서 bracketed paste로 wrap (event.rs:87).
- RuntimeEvent::PtyInputPressure — 폰 composer의 "입력 대기열 참" 표시 소스
  (event.rs:110, queued=0 해소 이벤트 포함).
- P4 푸시 인프라 — 딥링크(P6c)는 페이로드에 세션 id만 추가하면 된다.
- 데스크톱 이미지 paste 관례(4bf4901: 클립보드 → temp 파일 → 경로 paste) —
  P6d 업로드 흐름의 종단이 동일하다.
```

## PR-P6a — 입력 프로토콜 + 서버 경로 (runtime 무변경)

### 범위

```text
crates/web-remote: protocol.rs(ClientMsg::Input{session,text} + named key 확장 +
  ServerMsg::InputPressure), ws_api.rs(시청 게이트 + 입력 전용 상한), dashboard.rs
  (send_input: C0 strip → bracketed paste wrap → WriteInput, 세션별 paste 모드 캐시,
  PtyInputPressure 중계), http.rs(보안 헤더 2종), lib.rs(WS Origin 화이트리스트)
```

### 구현 요점

- `Input { session, text }`: C0 strip(\t 허용) → `\n`을 `\r`로 정규화. 텍스트에
  `\n`이 있거나 길이가 임계(예: 512B) 초과면 **paste로 간주**해 세션의 최신
  bracketed_paste 모드가 on일 때 `\x1b[200~ … \x1b[201~` wrap.
- named key 확장(Key 화이트리스트에 추가): `up/down/left/right/tab/esc/ctrl_d/
  shift_tab` — 서버가 이스케이프 시퀀스로 매핑. 클라는 이름만 보낸다.
- `ServerMsg::InputPressure { session, queued }`: 브리지가 시청 중 세션의
  PtyInputPressure를 중계 — composer 비활성/재활성 신호.
- Origin 검사: 업그레이드 요청에 Origin 헤더가 **있으면** 허용 목록(ts_hostname,
  localhost)과 대조해 불일치 시 403. 부재(비브라우저 클라)는 기존대로 토큰 인증에
  위임 — 기존 e2e 테스트(ws_client는 Origin 미전송)와 호환.

### 완료 기준

- 단위: C0 strip, \n→\r, paste 판정+wrap(모드 off면 wrap 없음), named key 매핑.
- e2e: 비시청 세션 Input 차단, 256KB 상한, Origin 불일치 403/부재 허용,
  InputPressure 프레임 왕복.

## PR-P6b — 폰 composer UI + 특수키 행

### 범위

```text
assets/: index.html(뷰어 하단 composer + 키 행), app.js(전송 계약 구현, IME,
  키보드 열림 레이아웃, pressure 처리), app.css
```

### 구현 요점

- composer: textarea(자동 높이, 최대 5행) + 전송 버튼. **Enter는 줄바꿈, 전송은
  버튼만**(모바일 관례 — 오전송 방지. 데스크톱 브라우저는 Cmd/Ctrl-Enter 전송).
- 전송 계약(0.2): 전송 시 `viewer.watching` 캡처, 낙관적 클리어하되 실패
  (ws 미연결/직후 절단) 시 draft 복원. InputPressure 수신 시 전송 버튼 비활성 +
  "대기열 참" 배지, queued=0에 해제.
- 특수키 행: `Esc · Tab · ↑ · ↓ · ← · → · Ctrl-C · Ctrl-D · Enter` — 누르면 즉시
  named key 전송(composer 미경유). 길게 눌러 반복(화살표) 지원.
- 키보드 열림 시 뷰어 canvas가 밀리지 않게 `visualViewport` resize 대응 —
  canvas 축소 대신 composer만 고정.
- 한글 IME: compositionend 이후 텍스트만 신뢰(중간 조합 전송 금지 — textarea라
  자연 충족, 명시 테스트 항목).

### 완료 기준

- 실기기: 한글 문장 입력→전송→에이전트 응답, **화살표+Enter로 claude code 메뉴
  선택**, 여러 줄 paste(bracketed) claude에 한 블록으로 도착, Ctrl-C 중단.
- iOS Safari/설치형 각각 키보드-composer 레이아웃 확인.

## PR-P6c — 알림 딥링크 + 승인·뷰어 연계 (P6a/b와 독립 — 병렬 가능)

### 범위

```text
crates/web-remote: push.rs(페이로드에 session id), assets/sw.js(notificationclick →
  '/?watch=<id>'), assets/app.js(URL 파라미터 자동 시청 + 승인 카드 "화면 보기")
```

### 구현 요점

- 푸시 페이로드에 `session` 추가(종류/제목/개수 최소 원칙 유지 — id는 민감 아님).
- SW notificationclick: 열린 창 있으면 focus+postMessage, 없으면
  `/?watch=<id>` open. app.js가 파라미터를 소비해 자동 openViewer.
- 승인 카드에 "화면 보기" 버튼(해당 세션 시청) — 승인 전 맥락 확인 흐름.
- **세션 id는 worker-로컬**(재시작 시 재배정): 자동 시청 전에 대시보드 세션
  목록에 id 존재를 확인, 없으면 조용히 무시(스테일 알림 딥링크 방어).

### 완료 기준

- 실기기: 입력대기 푸시 탭 → 앱 열림 → 해당 세션 뷰어 자동 표시 → composer로
  답변까지 한 손 흐름. 스테일 id 딥링크는 대시보드만 표시.

## PR-P6d — 이미지/파일 첨부

### 범위

```text
crates/web-remote: http.rs(POST /upload — 토큰 인증), 업로드 저장/정리,
assets/: composer 첨부 버튼(카메라/사진), 업로드 후 경로 텍스트 삽입
```

### 구현 요점

- `POST /upload?token=`: multipart 아님(단순 바이트 + Content-Type 검사, 이미지
  화이트리스트), 상한 10MB, `logs_root/uploads/<uuid>.<ext>`(또는 tmp) 저장,
  총량 예산 + 오래된 것 GC(scrollback_archive LRU 관례 재사용).
- 응답으로 로컬 경로 반환 → composer에 경로 삽입 → 사용자가 문맥과 함께 전송
  (데스크톱 이미지 paste와 동일 종단: 에이전트는 경로를 읽는다).
- 보안: 토큰 인증 필수, 경로는 서버가 생성(클라 지정 금지), 실행 권한 없음.

### 완료 기준

- 실기기: 폰 사진 → 업로드 → claude 세션에 "이 이미지 봐줘" + 경로 전송 →
  에이전트가 이미지 인식. 상한/타입 거부, GC 테스트.

## PR-P6e — 미결정·후순위 (실사용 후 결정)

- **폰에서 스폰**: 대시보드에서 새 셸/에이전트 시작(SpawnShell/SpawnAgent + 저장된
  agent_configs 선택). "폰만으로 작업 시작"의 마지막 조각 — env/cwd 선택 UX와
  suspended 가드 검토 필요.
- **raw 키 모드**: TUI(vim/htop) 대응 키 단위 전송. composer로 부족할 때만.
- **가독 랩 모드**(mux TerminalReadableGridView 차용): 80열 고정 canvas 대신 폰
  폭 재랩핑 읽기 토글.
- 세션 identity 영속화(워커-로컬 id 한계의 근본 해소) — 딥링크/재접속 신뢰성이
  더 필요해지면 v2.5 §11 영속 세션과 연결해 별도 설계.

## 순서·리소스·공통 완료 조건

- 순서: **P6a → P6b**(자유 입력 완결, 핵심 경로) ∥ **P6c**(독립, 병렬 가능) → P6d.
  P6e는 P6b~d 실사용 후.
- 리소스 예산(§14 관례): 입력 경로는 전부 이벤트 구동 — 유휴 추가 비용 0.
  InputPressure 중계는 시청 중에만. 업로드는 요청 시에만 디스크 I/O + 총량 예산.
- 공통: cargo fmt/clippy/check/test + xtask check-deps/i18n-check(신규 설정 문자열
  없음 — 웹 자산 한국어는 기존 관례), SW 캐시 버전은 자산 해시로 자동.

## 리스크

- iOS Safari의 visualViewport/키보드 레이아웃 편차 — P6b 실기기 왕복이 몇 번
  필요할 수 있다(회피책: composer를 페이지 하단 고정 + 뷰어 스크롤 분리).
- bracketed paste 모드 캐시의 초기값(false): 시청 시작 직후 첫 Viewport 전에
  paste하면 wrap이 빠질 수 있다 — lease 시작 즉시 push(P5a)가 오므로 실질 창은
  수백 ms. 문서화로 수용.
- 여러 줄 paste가 대형(수십 KB)일 때 PTY 큐 backpressure — InputPressure 중계가
  UI에 보이므로 사용자 인지 가능. 분할 전송은 비범위(데스크톱과 동일 동작).
