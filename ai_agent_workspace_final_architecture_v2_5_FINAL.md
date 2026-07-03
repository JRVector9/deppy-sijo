# Rust AI Agent Workspace 최종 설계문서 v2.5

작성일: 2026-07-03  
대상: Windows / macOS 우선, Linux 확장 가능  
제품 목표: 저 RAM / 저 CPU, 안정적 AI agent workspace, mux 확장, project env 관리, remote attach, MCP/OAuth 웹형 연결 UX  
상태: 개발 착수용 최종 확정본 — v2.4 + 독립 리뷰(2026-07-03, codex) Critical/High 반영: 모순 2건 해소, mux_tabs/migration/CHECK 제약 추가, MCP 최신 스펙 기준 갱신. 이 문서 하나로 self-contained.

---

## 0. 최종 결론

현재 구조는 **UI가 런타임/터미널/PTY 구현체와 분리되어 동작하는 구조**로 확정한다.

```text
App UI
 → RuntimeClient
 → Runtime Boundary
 → Mux Runtime
 → Session Runtime
 → TerminalBackend
 → PtyBackend
```

UI는 직접 `SessionManager`, `PtyBackend`, `alacritty_terminal`, `portable-pty` 타입을 참조하지 않는다.  
UI는 `RuntimeCommand`를 보내고 `RuntimeEvent`를 구독한다.

최종 제품 정의:

```text
AI Agent Workspace with Muxed Terminal Runtime
```

## 0.1 핵심 불변 원칙

아래 7개 원칙은 모든 PR에서 위반 금지다. 코드 리뷰 체크리스트로 사용한다.

```text
1. UI는 RuntimeClient만 본다.
2. Runtime은 mux / session / env / terminal / pty를 조율한다.
   (UI가 이들을 직접 조율하지 않는다)
3. Active pane만 render한다.
4. Hidden workspace/session은 log/status만 처리한다.
   (단, terminal backend의 grid state 유지는 허용 — reattach 화면 복원용.
    render/snapshot/glyph layout은 금지. 8.2, 14.2와 일치)
5. Terminal snapshot(TerminalViewportSnapshot)은 visible pane에만 만든다.
6. Raw log 평문은 기본 저장하지 않는다. (기본은 redacted log)
7. Session은 secret store를 직접 모른다.
   (secret은 EnvInjectionPolicy가 spawn 직전에만 resolve해서 주입)
```

최종 스택:

```text
Rust
eframe/egui
Runtime Boundary
Mux Runtime
Session Runtime
TerminalBackend abstraction
AlacrittyBackend first
LibGhosttyBackend later
portable-pty
dedicated PTY reader threads
tokio orchestration runtime
SQLite metadata
append-only redacted logs
OS keyring secret store
Project Environment Manager
local stdio MCP first
remote MCP / OAuth PKCE later
remote attach skeleton
```

---

## 1. 웹 검토 기반 수정 사항

## 1.1 eframe/egui

검토 결과, eframe은 egui용 framework crate이며, native와 web 앱 모두에 사용할 수 있다. 따라서 desktop-first로 가면서도 나중에 일부 web/WASM UI 실험이 가능하다.

설계 반영:

```text
- eframe/egui 유지
- app UI는 native-first
- browser/remote client는 별도 client로 분리
```

### egui 0.35 breaking change (2026-07-03 PR-00에서 실측 확인)

eframe 0.35부터 App trait 시그니처가 변경되었다.
웹 자료/예제/LLM 학습 데이터 대부분은 구 시그니처 기준이므로 구현 시 주의.

```text
구 (0.34 이하):
  fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame)
  egui::CentralPanel::default().show(ctx, |ui| ...)   // &Context를 받음

신 (0.35):
  fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame)
  egui::CentralPanel::default().show(ui, |ui| ...)    // &mut Ui를 받음

추가 (PR-01에서 실측 확인):
  SidePanel / TopBottomPanel 타입이 제거되고 egui::Panel로 통합됨
  → egui::Panel::top("id") / bottom / left / right 사용
  egui::Window는 여전히 show(&Context, ...) — ui.ctx()로 접근

적용 규칙:
- 모든 UI 코드는 신 시그니처 기준으로 작성한다
- Context가 필요하면 ui.ctx()로 접근한다 (request_repaint, set_theme 등)
- PR-05 TerminalRenderer: 참고 구현 egui_term(1.7)은 egui 0.34 구 시그니처
  기반이므로 렌더 루프를 그대로 복사하지 말고 &mut Ui 기준으로 이식한다
```

## 1.2 portable-pty

portable-pty는 시스템 PTY 인터페이스를 위한 cross-platform API이고, runtime에 따라 구현체를 선택할 수 있는 trait 구조를 제공한다. 예제도 `openpty`, `spawn_command`, reader/writer 구조를 사용한다.

설계 반영:

```text
- portable-pty 유지
- PTY read/write는 dedicated thread
- portable-pty 타입은 pty crate 내부에만 격리
- PtyBackend trait으로 감싼다
```

리스크 (2026-07 재검토 확정, wezterm 모노레포에서 활발히 관리 중이나 아래 명기):

```text
1. crates.io 릴리스 지연:
   0.9.0(2025-02) 이후 미릴리스. Windows kill() 수정(2026-06) 등이
   미포함 — 필요 시 git 의존성 또는 포크 전환 (PtyBackend trait이 격리)

2. Windows ConPTY 플래그:
   RESIZE_QUIRK / WIN32_INPUT_MODE / PASSTHROUGH_MODE 미적용으로
   Windows 10/11 resize 아티팩트 가능 — PR-04/PR-21에서 실측 확인

3. drop 순서 race (2026-03 보고):
   SlavePty가 MasterPty보다 오래 살면 handle 파괴가 비결정적
   → 규칙: spawn 직후 slave handle을 즉시 drop한다
```

## 1.3 alacritty_terminal

alacritty_terminal 0.26.0은 `Grid`, `Term`, `vte`를 re-export하고 `event_loop`, `tty` 모듈도 제공한다. 하지만 우리 앱에서는 `tty`와 `event_loop`를 쓰지 않는다.

설계 반영:

```text
사용:
  Term
  Grid
  vte parser
  terminal state

금지:
  alacritty_terminal::tty
  alacritty_terminal::event_loop
  alacritty ChildEvent 기반 exit status

이유:
  PTY는 portable-pty가 담당해야 하고,
  terminal backend와 process runtime을 결합하면 libghostty 교체성이 깨진다.
```

## 1.4 keyring-core

keyring-core 1.0.0은 다양한 secure credential store에 password/secret을 저장하기 위한 cross-platform abstraction을 제공한다. 단, mock/sample stores는 secure/robust하다고 보장되지 않는다. 기본 store는 `set_default_store()`로 지정할 수 있다.

설계 반영:

```text
- keyring-core + 플랫폼 store crate 사용
- mock/sample store는 test only
- production에서는 Windows Credential Manager / macOS Keychain / Linux Secret Service 계열만 허용
- keyring unavailable 시 insecure fallback은 기본 비활성
- 동시성: 동일 credential의 멀티스레드 접근은 신뢰 불가 —
  SecretStore 접근은 단일 actor(serialized access)로 제한한다
- macOS store는 default feature가 없으므로 feature(keychain 등)를 명시 선택한다
```

## 1.5 MCP transport / authorization

**기준 스펙: 2025-11-25 개정판** (구버전 2025-03-26 기준 금지. PR-15/17/18/19 시작 시점에 최신 개정판 재확인 필수)

transport spec은 stdio와 Streamable HTTP를 정의한다. stdio는 client가 server를 subprocess로 실행하고, server stdout에는 valid MCP message만 써야 한다. Streamable HTTP는 POST/GET과 optional SSE를 사용하며, local server는 DNS rebinding 방지를 위해 Origin 검증, localhost binding, authentication이 권장된다.

2025-11-25 개정판에서 주의할 변경점:

```text
- Streamable HTTP: POST body는 단일 JSON-RPC 메시지 (batch 전송 금지)
- initialize 이후 모든 HTTP 요청에 MCP-Protocol-Version 헤더 필수
- SSE polling/retry, 세션 보안 요구 강화
- OAuth: Protected Resource Metadata 기반 discovery,
  Client ID Metadata Documents 중심으로 변경
```

MCP authorization spec은 HTTP-based transport에서 OAuth 2.1 기반 authorization을 정의한다. STDIO transport는 이 authorization spec을 따르지 않고, credentials는 environment에서 가져오도록 되어 있다. PKCE는 모든 client에 required이며, redirect URI는 localhost 또는 HTTPS여야 한다.

설계 반영:

```text
v0:
  local stdio MCP only
  credentials from Project Environment Manager
  stdout protocol strictness 검증

v1:
  Streamable HTTP MCP
  Origin validation
  localhost binding default
  auth required by default

v1+ OAuth:
  external browser
  PKCE required
  state validation
  token keyring storage
  redirect URI localhost or HTTPS only
```

## 1.6 libghostty / libghostty-vt

libghostty-vt는 Ghostty에서 추출한 terminal emulation library의 Rust binding이며, terminal escape parsing, terminal state, input event encoding, scrollback, wrapping, resize reflow 등을 제공한다. 하지만 현재 API는 stable이 아니며, breaking changes 가능성이 있다. 또한 libghostty-vt 타입은 `!Send`/`!Sync`로 문서화되어 있어 thread 경계 설계가 중요하다.

설계 반영:

```text
- v0~v1은 AlacrittyBackend
- v1.x에서 LibGhosttyBackend experimental
- terminal backend는 worker thread confinement 가능하게 설계
- UI는 libghostty 타입 직접 참조 금지
```

## 1.7 참고 구현: egui_term

https://github.com/Harzu/egui_term (egui + alacritty_terminal 터미널 위젯)

```text
판정 (2026-07 검토 확정):
  포크 금지 / 직접 의존 금지 — 참고 전용

이유:
  - backend가 내장 tty + EventLoop에 강결합 (1.3의 금지 구조 실례)
  - 매 프레임 Grid 전체 clone, panic 경로, 테스트 부재
  - bold/italic/underline 렌더링, 커서 shape/blink 미구현

참고 가치 (PR-05 구현 시):
  - view.rs: egui에서 alacritty grid 렌더링 루프
  - theme.rs: 256색/truecolor 매핑
  - bindings.rs: 키 바인딩 표
```

## 1.8 확정 개발 스택 버전 (2026-07-03 crates.io 실측)

patch 버전은 문서에 박제하지 않는다. 재현성은 `Cargo.lock`으로 확보하고,
release 전 `cargo update / tree / audit / deny + smoke test`를 돌린다.

```toml
eframe = "0.35"
egui = "0.35"
alacritty_terminal = "0.26"
portable-pty = "0.9"
tokio = "1"
rusqlite = { version = "0.40", features = ["bundled"] }  # SQLite 버전 고정
serde = "1"
keyring-core = "1"
apple-native-keyring-store = "1"     # macOS
windows-native-keyring-store = "1"   # Windows
# Linux 확장 시: zbus-secret-service-keyring-store 또는 linux-keyutils-keyring-store
notify-rust = "4"
oauth2 = "5"
axum = "0.8"
```

```text
주의:
- keyring = "3"/"4" 단일 crate 사용 금지 (1.4 참조)
- eframe/egui는 업데이트가 빠름 — minor line 기준으로만 명시
- tokio/axum/oauth2 등의 feature set은 PR-00에서 최소 집합으로 확정
  (tokio full feature 금지 — rt-multi-thread, sync, process 등 필요한 것만)
- Rust edition 2024, rust-toolchain.toml로 stable 고정
```

---

## 2. UI 분리 구조

## 2.1 원칙

UI는 상태를 보여주고 명령을 보낸다.  
UI는 런타임 구현체를 직접 호출하지 않는다.

```text
금지:
  UI → SessionManager 직접 호출
  UI → portable-pty 직접 호출
  UI → alacritty_terminal 타입 직접 참조
  UI → SecretStore 직접 get_secret 호출

허용:
  UI → RuntimeCommand 전송
  UI ← RuntimeEvent 수신
  UI → TerminalViewportSnapshot / TerminalExternalSurfaceHandle 렌더링
       (4.2 TerminalRenderModel 참조 — UI가 보는 터미널 데이터는 이 둘뿐)
```

## 2.2 RuntimeClient

```rust
pub trait RuntimeCommandSink {
    fn send_command(&self, command: RuntimeCommand) -> anyhow::Result<()>;
}

pub trait RuntimeEventStream {
    fn subscribe(&self) -> RuntimeEventReceiver;
}

pub trait RuntimeClient: RuntimeCommandSink + RuntimeEventStream {}
```

## 2.3 구현체

```text
v0:
  InProcessRuntimeClient

v1:
  LocalhostRuntimeClient

v2:
  RemoteRuntimeClient
```

이 구조를 쓰면 remote attach는 나중에 “새 기능”이 아니라 `RuntimeClient` 구현체 교체가 된다.

---

## 3. 최종 아키텍처

```text
AI Agent Workspace with Muxed Terminal Runtime

 ├─ App UI: eframe/egui
 │   ├─ WorkspaceSidebar
 │   ├─ MuxTabBar
 │   ├─ MuxPaneView
 │   ├─ TerminalSurfaceView
 │   ├─ AgentStatusBar
 │   ├─ ProjectEnvPanel
 │   ├─ NotificationCenter
 │   ├─ SettingsPanel
 │   └─ ConnectorCenter
 │
 ├─ Runtime Boundary
 │   ├─ RuntimeClient trait
 │   ├─ RuntimeCommandSink trait
 │   ├─ RuntimeEventStream trait
 │   ├─ InProcessRuntimeClient
 │   └─ Future RemoteRuntimeClient
 │
 ├─ Mux Runtime
 │   ├─ MuxWorkspace
 │   ├─ MuxWindow
 │   ├─ MuxTab
 │   ├─ MuxPane
 │   ├─ LayoutTree
 │   ├─ FocusManager
 │   ├─ AttachDetachManager
 │   ├─ MuxEventRouter
 │   └─ MuxPersistence
 │
 ├─ Session Runtime
 │   ├─ AgentSession
 │   ├─ ShellSession
 │   ├─ McpSession
 │   ├─ SessionLifecycle
 │   ├─ SessionIO
 │   └─ SessionStatus
 │
 ├─ Terminal Runtime
 │   ├─ TerminalBackend trait
 │   ├─ AlacrittyBackend
 │   ├─ Future LibGhosttyBackend
 │   ├─ TerminalRenderModel
 │   ├─ TerminalViewportSnapshot
 │   ├─ TerminalChangeSet
 │   ├─ TerminalRenderer
 │   ├─ TerminalInputMapper
 │   ├─ TerminalSelection
 │   └─ TerminalClipboardBridge
 │
 ├─ PTY Runtime
 │   ├─ PtyBackend trait
 │   ├─ PortablePtyBackend
 │   ├─ PtyReaderThread
 │   ├─ PtyWriterHandle
 │   ├─ ResizeBridge
 │   └─ ProcessExitWatcher
 │
 ├─ Project Environment
 │   ├─ EnvProfileManager
 │   ├─ EnvVarRegistry
 │   ├─ EnvProfileRepository trait
 │   ├─ SecretEnvBinding
 │   ├─ ProjectServerRegistry
 │   ├─ EnvTemplate
 │   ├─ EnvDiffPreview
 │   ├─ EnvPrecedenceResolver
 │   └─ EnvInjectionPolicy
 │
 ├─ Storage
 │   ├─ SQLite metadata
 │   ├─ repositories
 │   ├─ migrations
 │   ├─ append-only redacted ANSI logs
 │   ├─ redacted plain text logs
 │   ├─ redacted event jsonl logs
 │   ├─ optional encrypted raw log
 │   └─ log index
 │
 ├─ Secret
 │   ├─ SecretStore trait
 │   ├─ KeyringSecretStore
 │   ├─ CredentialMetadataRepository
 │   ├─ RedactionService
 │   ├─ SecretScanner
 │   └─ SecretString
 │
 ├─ MCP
 │   ├─ LocalMcpServerManager
 │   ├─ StdioJsonRpcTransport
 │   ├─ RemoteMcpClient, v1+
 │   ├─ ToolRegistry
 │   ├─ PermissionPolicy
 │   ├─ ToolApprovalDialog model
 │   └─ AuditLog
 │
 ├─ Auth, v1+
 │   ├─ OAuthPKCE
 │   ├─ LocalhostCallbackServer
 │   ├─ BrowserLauncher
 │   ├─ OAuthStateStore
 │   └─ TokenStore
 │
 ├─ Remote Runtime, v1+
 │   ├─ HeadlessRuntime
 │   ├─ RemoteMuxServer
 │   ├─ RemoteAttachClient
 │   ├─ WebSocketGateway
 │   ├─ AuthenticatedSession
 │   └─ TransportProtocol
 │
 └─ Platform
     ├─ AppDirs
     ├─ Notifications
     ├─ ShellDefaults
     ├─ Clipboard
     ├─ OpenExternalBrowser
     └─ PackagingHelpers
```

---

## 4. libghostty 교체 가능성

## 4.1 결론

이 구조는 나중에 `libghostty`로 바꾸기 편한 구조다.  
단, 아래 조건을 반드시 지켜야 한다.

```text
- alacritty_terminal 타입을 app/core/mux/session/storage에 노출하지 않는다.
- TerminalBackend trait 뒤에만 둔다.
- PTY는 terminal backend가 아니라 PtyBackend가 소유한다.
- exit status는 terminal backend가 아니라 PtySessionHandle에서 나온다.
- UI는 TerminalRenderModel / TerminalViewportSnapshot / TerminalExternalSurfaceHandle만 본다.
- 로그와 status detector는 backend 내부가 아니라 SessionWorker output stream 기반으로 동작한다.
```

## 4.2 TerminalRenderModel

```rust
pub enum TerminalRenderModel {
    CellGrid,
    ExternalSurface,
}
```

```rust
pub trait TerminalBackend {
    fn feed(&mut self, bytes: &[u8]) -> anyhow::Result<TerminalChangeSet>;
    fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()>;
    fn render_model(&self) -> TerminalRenderModel;

    fn viewport_snapshot(&self) -> Option<TerminalViewportSnapshot>;
    fn external_surface(&self) -> Option<TerminalExternalSurfaceHandle>;

    fn scroll(&mut self, delta: i32);
    fn reset(&mut self);
}
```

## 4.3 Backend별 전략

```text
AlacrittyBackend:
  TerminalRenderModel::CellGrid
  egui_cell_renderer 사용

LibGhosttyBackend, Mode A:
  TerminalRenderModel::CellGrid
  libghostty-vt state를 snapshot으로 변환

LibGhosttyBackend, Mode B:
  TerminalRenderModel::ExternalSurface
  libghostty render surface를 egui/wgpu surface bridge로 표시
```

## 4.4 libghostty 주의

```text
- libghostty-vt는 API 안정성이 아직 낮다.
- breaking change 가능성이 있다.
- 타입이 !Send / !Sync일 수 있다.
- 별도 terminal emulation thread에 가두고 channel로 통신하는 구조가 필요할 수 있다.
```

따라서:

```text
v0~v1:
  AlacrittyBackend stable

v1.x:
  LibGhosttyBackend experimental

v2:
  backend 선택 가능
```

---

## 5. Mux Runtime

## 5.1 핵심 개념

```text
Session:
  실제 실행 중인 shell/agent process

Pane:
  session을 보여주는 화면 영역

Tab:
  여러 pane을 담는 화면 묶음

Workspace:
  프로젝트 단위 mux container

LayoutTree:
  split 구조를 표현하는 트리
```

## 5.2 Mux 객체 모델

```rust
pub struct MuxWorkspace {
    pub id: WorkspaceId,
    pub name: String,
    pub root_path: PathBuf,
    pub windows: Vec<MuxWindowId>,
}

pub struct MuxWindow {
    pub id: MuxWindowId,
    pub tabs: Vec<MuxTabId>,
    pub active_tab: Option<MuxTabId>,
}

pub struct MuxTab {
    pub id: MuxTabId,
    pub title: String,
    pub layout: LayoutNode,
    pub active_pane: Option<MuxPaneId>,
}

pub struct MuxPane {
    pub id: MuxPaneId,
    pub session_id: Option<SessionId>,
    pub title: String,
    pub pane_kind: PaneKind,
}
```

## 5.3 LayoutTree

```rust
pub enum LayoutNode {
    Pane(MuxPaneId),
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

pub enum SplitDirection {
    Horizontal,
    Vertical,
}
```

## 5.4 Attach / Detach 단계

```text
v0:
  one session → one pane

v1:
  session detach/reattach 지원

v1.5:
  one session → multiple views 검토

v2:
  desktop/browser/remote client가 같은 session에 attach
```

---

## 6. Project Environment Manager

## 6.1 목적

AI agent 작업 중 환경변수 실수를 줄인다.

대표 실수:

```text
- API Key를 .env에 평문 저장
- agent가 로그에 secret 출력
- production DB URL을 local task에 주입
- 프로젝트 A의 API Key가 프로젝트 B에 들어감
- 배포 서버 URL을 잘못 입력
- .env.local, .env.production, shell env가 섞임
- MCP server와 agent가 서로 다른 환경변수를 봄
```

## 6.2 Env Precedence

낮음 → 높음:

```text
1. OS inherited env
2. Workspace default env
3. EnvProfile env
4. AgentConfig env
5. Session one-shot env
6. MCP server scoped env
7. Manual launch override
```

충돌 시 UI에서 보여준다.

```text
ANTHROPIC_API_KEY
  workspace: cred_old
  profile:   cred_new
  result:    cred_new
```

## 6.3 Secret 저장 정책

```text
Plain env:
  SQLite/config 저장 가능

Secret env:
  keyring 저장
  SQLite에는 credential_id와 masked_hint만 저장

Runtime:
  session spawn 직전에 keyring에서 읽어 env에 주입

Logs:
  secret 값은 RedactionService에 등록
```

## 6.4 Production Guard

```text
production profile:
  - 실행 전 경고
  - workspace allowlist 가능
  - agent별 차단 가능
  - MCP server로 주입 시 별도 경고
```

## 6.5 .env 파일 정책

```text
지원:
  - .env import
  - .env compare
  - .env.example generate
  - missing env detection

금지:
  - secret을 .env에 자동 저장
  - production secret을 plain file로 export
```

---

## 7. Logging 정책

기본 로그는 반드시 redacted log다.

```text
logs/
 └─ workspace_id/
     └─ session_id/
         ├─ redacted.ansi.log
         ├─ redacted.plain.txt
         └─ events.redacted.jsonl
```

선택 기능:

```text
optional:
  encrypted.raw.ansi.log
```

조건:

```text
- 기본 비활성
- 사용자가 명시적으로 활성화
- keyring 기반 encryption key 사용
- export 시 별도 경고
```

Redaction 대상:

```text
- API key
- OAuth access token
- refresh token
- bearer token
- GitHub token
- database URL
- SSH private key
- MCP tool input secret
- env secret value
```

Redaction 구현 방향 (세부는 PR-11/16/22에서 확정):

```text
- streaming redaction: chunk 경계에 걸린 secret 대응을 위해
  최대 secret 길이만큼 lookbehind buffer 유지
- 매칭 전 ANSI escape sequence 제거 후 검사 (원문에는 escape 삽입 가능)
- secret의 base64 / URL-encoded / JSON-escaped 변형도 패턴에 등록
- token refresh 시 새 token을 즉시 RedactionService에 등록
- redaction 불확실(패턴 엔진 오류 등) 시 해당 로그 세그먼트는
  기록하지 않고 gap marker만 남긴다 (보수적 폐기)
- encrypted raw log / input_encrypted_blob: AEAD 사용,
  key는 keyring 보관, key id를 함께 저장, rotation 시 재암호화 없이 key id로 구분
```

---

## 8. Terminal Runtime 성능 정책

## 8.1 Full clone 방지

```rust
pub struct TerminalViewportSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor: CursorSnapshot,
    pub visible_cells: Arc<[TerminalCell]>,
    pub dirty_ranges: Vec<CellRange>,
    pub title: Option<String>,
    pub scroll_offset: i32,
    pub is_alt_screen: bool,
}
```

```rust
pub struct TerminalChangeSet {
    pub dirty_rows: Vec<u16>,
    pub cursor_changed: bool,
    pub title_changed: bool,
    pub bell: bool,
}
```

## 8.2 성능 규칙

```text
active pane:
  viewport snapshot 생성 가능

hidden pane:
  snapshot clone 금지
  terminal state update only
  log/status only

remote:
  output event와 terminal delta를 분리
```

---

## 9. Crate 구조

```text
agent-workspace/
 ├─ crates/
 │   ├─ app/
 │   ├─ core/
 │   ├─ runtime/
 │   │   ├─ command.rs
 │   │   ├─ event.rs
 │   │   ├─ client.rs
 │   │   ├─ in_process.rs
 │   │   └─ router.rs
 │   │
 │   ├─ mux/
 │   │   ├─ workspace.rs
 │   │   ├─ window.rs
 │   │   ├─ tab.rs
 │   │   ├─ pane.rs
 │   │   ├─ layout_tree.rs
 │   │   ├─ focus.rs
 │   │   ├─ attach.rs
 │   │   ├─ events.rs
 │   │   └─ persistence.rs
 │   │
 │   ├─ session/
 │   │   ├─ session.rs
 │   │   ├─ lifecycle.rs
 │   │   ├─ agent_session.rs
 │   │   ├─ shell_session.rs
 │   │   ├─ mcp_session.rs
 │   │   ├─ io.rs
 │   │   └─ status.rs
 │   │
 │   ├─ terminal/
 │   │   ├─ backend.rs
 │   │   ├─ alacritty_backend.rs
 │   │   ├─ libghostty_backend.rs
 │   │   ├─ viewport_snapshot.rs
 │   │   ├─ change_set.rs
 │   │   ├─ renderer_egui.rs
 │   │   ├─ external_surface.rs
 │   │   ├─ input_mapper.rs
 │   │   └─ selection.rs
 │   │
 │   ├─ pty/
 │   ├─ env/
 │   │   ├─ profile.rs
 │   │   ├─ env_var.rs
 │   │   ├─ resolver.rs
 │   │   ├─ precedence.rs
 │   │   ├─ injection_policy.rs
 │   │   ├─ server_registry.rs
 │   │   ├─ dotenv_import.rs
 │   │   ├─ diff_preview.rs
 │   │   ├─ safety.rs
 │   │   └─ repository.rs
 │   │
 │   ├─ storage/
 │   │   ├─ repositories/
 │   │   │   ├─ env_profile_repo.rs
 │   │   │   ├─ env_var_repo.rs
 │   │   │   ├─ mux_repo.rs
 │   │   │   ├─ session_repo.rs
 │   │   │   └─ audit_repo.rs
 │   │   └─ logs/
 │   │       ├─ redacted_ansi_writer.rs
 │   │       ├─ redacted_plain_text_writer.rs
 │   │       ├─ redacted_event_writer.rs
 │   │       ├─ encrypted_raw_writer.rs
 │   │       └─ rotation.rs
 │   │
 │   ├─ secret/
 │   ├─ mcp/
 │   ├─ auth/
 │   ├─ remote/
 │   └─ platform/
```

---

## 10. Crate 의존 방향

```text
app
 ├─ runtime
 ├─ core
 ├─ platform
 └─ UI only

runtime
 ├─ core
 ├─ mux
 ├─ session
 ├─ env
 ├─ mcp
 ├─ storage
 └─ secret

mux
 ├─ core
 └─ no UI dependency

session
 ├─ core
 ├─ pty
 ├─ terminal
 └─ no secret dependency

env
 ├─ core
 ├─ secret trait
 └─ no storage implementation dependency

terminal
 ├─ core
 └─ no app dependency

storage
 ├─ core
 └─ repository implementations

mcp
 ├─ core
 ├─ storage
 └─ no app dependency

remote
 ├─ core
 ├─ runtime
 └─ no app dependency
```

금지:

```text
- core가 egui 참조 금지
- mux가 egui 참조 금지
- session이 app 참조 금지
- session이 secret store 직접 참조 금지
- env가 storage 구현체 직접 참조 금지
- terminal backend 타입이 UI 전역 노출 금지
- secret 값 Debug 출력 금지
```

---

## 11. DB 스키마 (전체 확정본)

이 문서 하나로 self-contained하도록 전체 스키마를 기술한다. (v1 문서 폐기됨)

## 11.0 기본 테이블

```sql
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE agent_configs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    command TEXT NOT NULL,
    args_json TEXT NOT NULL,
    env_json TEXT,
    env_credentials_json TEXT,
    waiting_regex TEXT,
    approval_regex TEXT,
    error_regex TEXT,
    done_regex TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE credentials (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    label TEXT NOT NULL,
    credential_kind TEXT NOT NULL,
    keyring_service TEXT NOT NULL,
    keyring_username TEXT NOT NULL,
    masked_hint TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_used_at TEXT
);

CREATE TABLE notifications (
    id TEXT PRIMARY KEY,
    workspace_id TEXT,
    session_id TEXT,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at TEXT NOT NULL,
    read_at TEXT
);

CREATE TABLE mcp_servers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    command TEXT,
    args_json TEXT,
    url TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE mcp_tools (
    id TEXT PRIMARY KEY,
    server_id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    input_schema_json TEXT,
    trust_level TEXT NOT NULL DEFAULT 'unknown',
    schema_hash TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(server_id) REFERENCES mcp_servers(id)
);
```

참고: agent_configs의 `*_regex` 컬럼은 stream regex와 snapshot 패턴 검색에
공용으로 사용한다 (PR-12 참조).

## 11.1 sessions

`sessions.layout_json`은 두지 않는다.  
layout source of truth는 `mux_windows` / `mux_layouts` / `mux_panes`다.

```sql
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    session_kind TEXT NOT NULL DEFAULT 'agent',  -- agent | shell | mcp
    agent_id TEXT,                               -- session_kind = agent일 때만
    title TEXT NOT NULL,
    command TEXT NOT NULL,
    args_json TEXT NOT NULL,
    cwd TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_log_offset INTEGER DEFAULT 0,
    CHECK (session_kind != 'agent' OR agent_id IS NOT NULL),
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY(agent_id) REFERENCES agent_configs(id)
);
```

## 11.2 mux_windows

`MuxWindow` 객체 모델의 영속화 테이블 (PR-14). tab 순서는 `mux_tabs.tab_index`가 담당한다.

```sql
CREATE TABLE mux_windows (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    title TEXT,
    active_tab_id TEXT REFERENCES mux_tabs(id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id)
);
```

## 11.3 mux_tabs

`MuxTab` 객체 모델의 영속화 테이블. window → tab 소속/순서의 source of truth.

```sql
CREATE TABLE mux_tabs (
    id TEXT PRIMARY KEY,
    window_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    title TEXT NOT NULL,
    tab_index INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(window_id, tab_index),
    FOREIGN KEY(window_id) REFERENCES mux_windows(id),
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id)
);
```

## 11.4 mux_layouts

```sql
CREATE TABLE mux_layouts (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    tab_id TEXT NOT NULL,
    layout_json TEXT NOT NULL,
    active_pane_id TEXT REFERENCES mux_panes(id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(tab_id),
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY(tab_id) REFERENCES mux_tabs(id)
);
```

## 11.5 mux_panes

```sql
CREATE TABLE mux_panes (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    tab_id TEXT NOT NULL,
    session_id TEXT,
    title TEXT NOT NULL,
    pane_kind TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id),
    FOREIGN KEY(tab_id) REFERENCES mux_tabs(id),
    FOREIGN KEY(session_id) REFERENCES sessions(id)
);
```

참고: `mux_windows.active_tab_id` ↔ `mux_tabs.window_id`는 상호 참조이므로
insert 순서는 window(active_tab_id NULL) → tabs → window update로 한다.

## 11.6 env_profiles / env_vars

Project Environment Manager(6장)의 영속화 테이블.  
secret env는 값 대신 `credential_id`만 저장한다 (0.1 원칙 7).

```sql
CREATE TABLE env_profiles (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'custom',   -- local | staging | production | custom
    is_production INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id)
);

CREATE TABLE env_vars (
    id TEXT PRIMARY KEY,
    profile_id TEXT NOT NULL,
    key TEXT NOT NULL,
    kind TEXT NOT NULL,                    -- plain | secret
    plain_value TEXT,                      -- kind = plain일 때만
    credential_id TEXT,                    -- kind = secret일 때 credentials.id 참조
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(profile_id, key),
    CHECK (kind IN ('plain', 'secret')),
    CHECK (kind != 'secret' OR (plain_value IS NULL AND credential_id IS NOT NULL)),
    CHECK (kind != 'plain' OR credential_id IS NULL),
    FOREIGN KEY(profile_id) REFERENCES env_profiles(id),
    FOREIGN KEY(credential_id) REFERENCES credentials(id)
);
```

```text
제약은 DDL의 CHECK + repository 계층에서 이중 강제한다.
```

6.2의 나머지 precedence 계층 영속 정책:

```text
- Workspace default env: env_profiles의 kind = 'workspace-default' profile로 저장
- AgentConfig env: agent_configs.env_json / env_credentials_json (기존)
- Session one-shot env / Manual launch override: 영속하지 않는다.
  실행 시점 key 목록만 (값 제외, secret은 credential_id만) audit log에 기록
- MCP server scoped env: mcp_servers에 env_json / env_credentials_json 컬럼을
  PR-15에서 추가 (agent_configs와 동일 규칙)
```

## 11.7 tool_audit_logs

`input_json` 평문 저장은 금지한다.

```sql
CREATE TABLE tool_audit_logs (
    id TEXT PRIMARY KEY,
    workspace_id TEXT,
    session_id TEXT,
    server_id TEXT,
    tool_name TEXT NOT NULL,
    input_redacted_json TEXT,
    input_encrypted_blob BLOB,
    decision TEXT NOT NULL,
    created_at TEXT NOT NULL
);
```

## 11.8 indexes

```sql
CREATE INDEX idx_sessions_workspace_id ON sessions(workspace_id);
CREATE INDEX idx_mux_windows_workspace_id ON mux_windows(workspace_id);
CREATE INDEX idx_mux_tabs_window_id ON mux_tabs(window_id);
CREATE INDEX idx_mux_panes_session_id ON mux_panes(session_id);
CREATE INDEX idx_env_vars_profile_key ON env_vars(profile_id, key);
CREATE INDEX idx_mcp_tools_server_name ON mcp_tools(server_id, name);
CREATE INDEX idx_tool_audit_workspace_id ON tool_audit_logs(workspace_id);
```

## 11.9 Migration / DB 운영 정책

```text
- migrations/ 디렉터리의 순차 SQL + PRAGMA user_version 기반 forward-only migration
- 앱 시작 시 pending migration 자동 적용, 실패 시 기동 중단 + DB 백업 파일 안내
- 모든 연결에 PRAGMA journal_mode=WAL, PRAGMA foreign_keys=ON 강제
- 파괴적 변경(컬럼 삭제/타입 변경)은 copy-table 방식
  (new table 생성 → copy → rename)
- rollback migration은 지원하지 않는다 (forward-only).
  release 전 "구버전 DB → 최신" migration 테스트를 xtask smoke에 포함
- migration 적용 전 DB 파일 자동 백업 (직전 1개 유지)
```

---

## 12. PR 계획 최종 검토본

## PR-00 — Project Bootstrap

```text
Cargo workspace, eframe 빈 앱, app dirs/logging/config 초기화
```

완료 기준:

```text
- Windows/macOS cargo run 성공
- 빈 창 표시
- config/data/log 경로 생성
- fmt/clippy 통과
```

## PR-01 — Settings Shell

```text
Settings 화면 골격, terminal/performance 설정
```

완료 기준:

```text
- 설정 저장/로드
- UI 설정 일부 hot reload
```

## PR-02 — Secret Store & Credential UI

```text
keyring-core + 플랫폼 store, API Key/token 저장, redaction
```

완료 기준:

```text
- secret은 keyring에만 저장
- SQLite/config/log에 평문 없음
- Debug 출력 REDACTED
```

## PR-03 — Project Environment Manager

```text
프로젝트별 env profile, plain env, secret env binding,
server registry, env precedence, env diff preview, production guard
```

완료 기준:

```text
- local/staging/production profile 생성
- secret env는 credential_id 참조
- production profile 실행 전 경고
```

순서 주의:

```text
PR-03 시점에는 spawn 경로가 없어 env injection을 실제 검증할 수 없다.
PR-03은 모델/UI/저장/precedence resolver까지만 완료 기준으로 하고,
spawn 시 injection + 로그 미노출 검증은 PR-09 완료 기준에 포함한다.
```

## PR-04 — Single Shell PTY

```text
portable-pty shell 실행, reader thread, writer handle
```

완료 기준:

```text
- Windows PowerShell
- macOS zsh
- 입력/출력/Ctrl+C
- spawn 직후 slave handle 즉시 drop (1.2 리스크 3)
```

순서 주의 (원칙 1/2 보호):

```text
PR-04~05에서 UI가 임시로 PTY/terminal에 직결하는 코드는
과도기 코드임을 주석으로 명시하고, PR-06에서 RuntimeClient 경유로
전면 대체·삭제한다. PR-06 완료 기준에 해당 직결 코드 0건 확인을 포함한다.
```

## PR-05 — Terminal Backend Abstraction + AlacrittyBackend

```text
TerminalBackend trait, TerminalChangeSet, TerminalViewportSnapshot, egui renderer
```

주의: renderer는 egui 0.35 신규 시그니처(App::ui / &mut Ui) 기준으로 작성한다 (1.1 참조).

완료 기준:

```text
- ANSI color
- cursor
- resize
- wide char 렌더링 (한글 2셀 폭, 깨짐 없음)
- IME/한글 입력 (조합 중 텍스트 표시 포함)
- bracketed paste
- alacritty_terminal 타입 UI 노출 없음
```

백로그 (PR-21까지 검증):

```text
- mouse reporting 커버리지 (wheel/right/middle)
- OSC 52 clipboard
- OSC 8 hyperlink
- alt-screen scrollback 동작
- selection copy semantics (줄바꿈/랩/wide char)
```

## PR-06 — Runtime Boundary

```text
RuntimeClient, RuntimeCommand, RuntimeEvent, InProcessRuntimeClient
```

완료 기준:

```text
- UI가 RuntimeClient로만 명령 전송
- UI가 SessionManager 직접 호출하지 않음
- PR-04~05의 UI 직결 과도기 코드 삭제 완료 (0건 확인)
```

## PR-07 — Mux Runtime

```text
MuxWorkspace, MuxTab, MuxPane, LayoutTree, FocusManager
```

완료 기준:

```text
- pane/session 분리
- layout source of truth는 mux
```

## PR-08 — Session Runtime

```text
AgentSession/ShellSession lifecycle, attach/detach 준비
```

완료 기준:

```text
- session lifecycle 명확화
- session은 secret store 직접 참조 금지
```

## PR-09 — Agent Command Registry

```text
agent command 등록, env profile 선택, secret env injection
```

완료 기준:

```text
- command + args array 사용
- env secret은 spawn 직전에만 resolve
```

## PR-10 — Multi Session Tabs & Panes

```text
mux pane에 session attach, tab/split UI, active pane render
```

완료 기준:

```text
- 3개 이상 세션 실행
- active pane만 render
```

## PR-11 — Append-only Redacted Logs

```text
redacted.ansi.log, redacted.plain.txt, events.redacted.jsonl,
optional encrypted raw log
```

완료 기준:

```text
- raw 평문 로그 기본 저장 금지
- secret scan 테스트 통과
```

## PR-12 — Status Detector

```text
stream regex + 화면 텍스트 패턴 검색 + output idle heuristic 3단 병행
```

원칙 5와의 정합 (중요):

```text
- visible pane: TerminalViewportSnapshot의 텍스트로 패턴 검색 가능
- hidden session: snapshot 생성 금지(원칙 5) —
  대신 SessionWorker가 terminal backend의 grid 텍스트를 직접 조회
  (경량 read-only 접근, snapshot/render 아님) + stream regex + idle heuristic
- 감지 주기는 output batch 단위 (매 frame 아님)
```

완료 기준:

```text
- 일반 출력에서 stream line regex로 상태 감지
- TUI agent(visible/hidden 모두)에서 화면 텍스트 패턴으로 상태 감지
- hidden session 감지 경로에서 TerminalViewportSnapshot 미생성 확인
- process exit status 반영 (portable-pty ExitStatus 기준)
```

## PR-13 — Notifications

```text
OS notification, internal notification center
```

완료 기준:

```text
- waiting/done/error 알림
- session focus
```

## PR-14 — Workspace / Restore

```text
workspace metadata, mux layout restore, session metadata restore, crash recovery
```

완료 기준:

```text
- mux layout 복원
- session metadata 복원
- crash recovery:
  - lock file로 중복 실행 방지
  - 앱 시작 시 session 상태 reconcile (orphan process → Exited 처리)
  - log offset 검증 (partial write 시 마지막 유효 지점으로 복구)
  - 비정상 종료 후 재시작 테스트 통과
```

## PR-15 — Local MCP Manager

```text
local stdio MCP server, initialize, tools/list
```

완료 기준:

```text
- stdout에는 valid MCP message만 허용
- stderr log capture/redaction
```

## PR-16 — Tool Permission & Audit

```text
approval dialog, permission policy, schema hash, redacted/encrypted audit log
```

완료 기준:

```text
- tool input redacted 저장
- encrypted blob은 선택
```

## PR-17 — Connector Center

```text
브라우저형 연결 UX, local MCP card, OAuth placeholder
```

완료 기준:

```text
- local MCP 쉽게 추가
- 연결 상태 표시
```

## PR-18 — OAuth PKCE Connector

```text
external browser, PKCE, localhost callback, keyring token
```

완료 기준:

```text
- PKCE 필수
- state 검증
- redirect URI localhost/HTTPS
- token keyring 저장
```

## PR-19 — Remote Transport Skeleton

```text
localhost-only runtime attach, protocol skeleton, no public remote yet
```

완료 기준:

```text
- InProcessRuntimeClient와 같은 명령/이벤트 모델 사용
- localhost-only attach 가능
```

## PR-20 — Packaging

```text
Windows MSI, macOS app bundle, smoke test
```

도구 검토 메모 (2026-07):

```text
- 기본안: cargo-wix(Windows) + 수동 macOS bundle
- 검토 대안: cargo-packager (CrabNebula) — Windows MSI(WiX) +
  macOS app bundle/dmg + Linux(deb/AppImage)를 한 도구로 커버
- CI 주의: GitHub Actions windows-latest는 WiX v3(legacy)만 내장
```

완료 기준:

```text
- Windows 설치 후 실행
- macOS .app 실행
```

## PR-21 — Performance Hardening

```text
output batching, hidden pane render guard, memory limits
```

완료 기준 (측정 조건: release build, 기준 하드웨어를 PR 시작 시 명시):

```text
- 빈 앱 RAM 150MB 이하, idle CPU 3% 이하 (idle repaint 0회 확인)
- hidden session 10개 + 그중 3개 대량 출력(10MB/min) 시
  active pane frame time p95 16ms 이하
- 대량 출력 세션 1개(cat 대용량 파일)에서 UI 응답성 유지
- 수치 미달 시 원인 프로파일 결과를 PR에 첨부
```

## PR-22 — Security Hardening

```text
secret scan, env leak scan, MCP permission hardening
```

완료 기준:

```text
- secret이 DB/config/log에 없음
- redaction corpus 테스트 통과:
  chunk 경계 분할 / ANSI escape 삽입 / base64 / URL-encoded /
  JSON-escaped 변형 각각에 대한 fixture 포함 (7장 정책 검증)
- MCP tool 변경 시 재승인
- 보안 체크리스트 완성
```

---

## 13. 최종 로드맵

```text
v0.1:
  eframe + portable-pty + shell

v0.2:
  secret store + project env manager

v0.3:
  terminal backend + runtime boundary + mux runtime

v0.4:
  agent session + tabs/panes

v0.5:
  redacted logs/status/notifications

v0.6:
  workspace restore

v0.7:
  local MCP + permission/audit

v0.8:
  connector center

v1.0:
  OAuth PKCE + packaging

v1.2:
  remote transport skeleton

v1.x:
  libghostty backend 검토
  remote MCP 고도화
  browser/remote client 확장
```

---

## 14. Workspace / Session Resource Policy

AI agent 사용자는 여러 창, 여러 workspace, 여러 agent 세션을 동시에 띄워놓는 경우가 많다.  
따라서 리소스 정책은 기능이 아니라 핵심 안정성 요구사항이다.

## 14.1 Workspace Runtime State

```rust
pub enum WorkspaceRuntimeState {
    Closed,      // DB metadata only
    Suspended,   // layout/session metadata only
    Warm,        // status/log tail only
    Active,      // visible panes can render
}
```

정책:

```text
Closed:
  - DB metadata만 유지
  - terminal backend 없음
  - PTY process 없음

Suspended:
  - workspace layout/session metadata만 메모리 유지
  - terminal snapshot 없음
  - render cache 없음

Warm:
  - session status / notification / recent log tail만 유지
  - terminal renderer 실행 금지
  - TerminalViewportSnapshot 생성 금지

Active:
  - 현재 보이는 tab/pane만 render
  - active visible pane만 TerminalViewportSnapshot 생성
```

## 14.2 Session Runtime State

```rust
pub enum SessionRuntimeState {
    RunningVisible,
    RunningHidden,
    Detached,
    Exited,
    Archived,
}
```

정책:

```text
RunningVisible:
  - PTY read
  - terminal backend update
  - redacted log append
  - status detect
  - active visible pane render

RunningHidden:
  - PTY read
  - terminal backend grid state update (reattach 화면 복원용,
    hidden scrollback 제한 14.3 적용 — render/snapshot은 하지 않음)
  - redacted log append
  - status detect
  - TerminalViewportSnapshot 생성 금지
  - render event coalesce/drop 가능

Detached:
  - process는 살아있을 수 있음
  - UI pane과 연결 없음
  - attach 가능

Exited:
  - process 종료
  - logs/index/search 가능
  - terminal backend는 일정 시간 후 drop 가능

Archived:
  - terminal backend drop
  - render cache drop
  - redacted logs/index만 유지
```

## 14.3 Memory Budget

기본 정책:

```text
Visible session:
  - scrollback 10,000 lines 또는 16MB 중 먼저 도달하는 값

Hidden session:
  - scrollback 1,000 lines 또는 2MB
  - 오래 hidden 상태면 render cache drop

Archived session:
  - terminal state drop
  - logs/index only
```

## 14.4 Render Budget

```text
절대 금지:
  - 모든 workspace/pane 매 frame paint
  - hidden pane TerminalViewportSnapshot 생성
  - hidden tab glyph layout
  - background workspace repaint

허용:
  active window
   → active workspace
    → visible tab
     → visible pane
      → dirty rows/ranges만 paint
```

## 14.5 Backpressure Policy

AI agent, build log, test runner, MCP server output이 폭주할 수 있으므로 모든 output queue는 bounded channel을 사용한다.

```text
PTY output queue:
  - bounded channel

우선순위:
  1. redacted log writer
  2. status detector
  3. active visible terminal update
  4. hidden render event

queue pressure 발생 시:
  - log writer는 보존 우선
  - active pane terminal update는 batch/coalesce
  - hidden session render event는 drop 가능
  - status detector는 line batch 단위로 처리
```

병목 시 동작 방향 (세부 수치는 PR-21에서 확정):

```text
- render event는 무제한 drop/coalesce 가능 (화면은 최신 상태만 필요)
- log writer가 지속 병목이면 마지막 수단으로 PTY read를 일시 중단한다.
  OS pipe buffer가 차면 child process의 write가 블록되는
  자연 backpressure를 사용한다 (데이터 유실 없음, child는 느려질 뿐)
- log를 drop하고 read를 계속하는 방식은 금지 (append-only 보존 원칙 위반)
- PTY read 중단이 일정 시간을 넘으면 UI에 back-pressure 상태 badge 표시
```

## 14.6 Stability Rules

```text
- Active workspace만 full render 대상이다.
- Warm/Suspended workspace는 terminal renderer와 snapshot을 만들지 않는다.
- RunningHidden session의 PTY output은 terminal grid state update / redacted log / status detector로만 전달한다 (render/snapshot 금지).
- Hidden session은 TerminalViewportSnapshot 생성을 금지한다.
- 오래 hidden 상태인 session은 terminal backend render cache를 drop할 수 있다.
- Exited/Archived session은 terminal backend를 drop하고 logs/index만 유지한다.
- 모든 output queue는 bounded channel로 제한한다.
- queue pressure 발생 시 render event는 coalesce/drop 가능하지만 log writer는 보존 우선이다.
```

---

## 15. 최종 판단

이 구조는 다음 목적에 맞다.

```text
- UI와 runtime 분리
- RAM/CPU 적게 사용
- AI agent 최적화
- mux 확장 가능
- 프로젝트별 환경변수 실수 방지
- redacted logging 기본값
- MCP/OAuth 웹형 UX
- remote attach 가능성
- libghostty 교체 가능성
```

최종 리스크:

```text
- egui terminal rendering 품질을 직접 끌어올려야 함
- libghostty backend는 API 안정성/스레드 제약 확인 필요
- Windows 알림 클릭 액션은 별도 wrapper 필요 가능
- Linux keyring fallback 이슈 가능
- portable-pty crates.io 릴리스 지연 / ConPTY 플래그 이슈 (1.2 참조)
- MCP 스펙 개정 주기가 빠름 — PR-15/17/18/19 시작 시 최신 개정판 재확인 (1.5 참조)
- remote attach 보안 모델은 public remote 전에 별도 설계 필요
```

최종 결론:

```text
개발 착수 가능.
이 문서(v2.5)가 유일한 기준 문서다. 이전 버전(v1, v2.x)은 폐기한다.
모든 PR은 0.1 핵심 불변 원칙 7개를 위반하지 않아야 한다.
특히 PR-06 Runtime Boundary와 PR-11 Redacted Logs는
이 문서의 2장(UI 분리)과 7장(Logging 정책) 그대로 구현한다.
```
