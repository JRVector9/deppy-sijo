//! WS JSON 프로토콜 v1 — 브라우저 친화 텍스트 프레임 (계획 v3.3 P2).
//!
//! postcard 바이너리 코덱은 데스크톱 원격(remote.rs) 전용으로 남기고, 폰 PWA는 JSON만
//! 쓴다. 첫 프레임은 반드시 `{"type":"auth", ...}` — 인증 전 다른 메시지는 무시된다.
//!
//! 서버→클라 프레임은 대시보드(런타임 유래)와 승인(DB 유래)이 갱신 주기가 달라 분리해
//! 보낸다. 클라이언트는 각 프레임을 독립적으로 반영한다.

use serde::{Deserialize, Serialize};

/// **세션 식별자는 영속 UUID다** (v3.7 I1): worker-로컬 u64는 워커마다 1부터 재배정되어
/// 워크스페이스 전환·재시작 시 다른 세션을 가리킬 수 있다(앨리어싱). 폰은 u64를 아예
/// 모르고, 서버가 UUID를 현재 워커의 u64로 변환한다 — 모르는 UUID면 명령이 만들어지지
/// 않는다.
///
/// 프로토콜 버전 — 클라/서버 합의값. 하위호환이 깨지면 증가시킨다.
///
/// v2 (2026-07-12): Dashboard 프레임이 `sessions[]` → `workspaces[]`로 바뀌었다.
/// 오래 열려 있던 옛 페이지가 재연결하면 세션 목록이 빈 채로 남으므로, 클라이언트가
/// welcome의 v를 확인해 불일치 시 스스로 재로드한다 (리뷰 P3-6).
/// v3 (2026-07-12): 워크스페이스 상태 와이어 값 `"idle"` → `"suspended"`(I1b-1). 옛 캐시
/// 클라는 라벨 맵/CSS에 이 값이 없어 영어 "suspended"를 그대로 표시하므로(graceful하나
/// 미번역), 버전 불일치로 재로드시켜 새 자산을 받게 한다 (리뷰 I1b-1 P3).
/// v4: owner-cell grapheme boundaries for canvas text advance.
/// v5: explicit direct text and named keys; resize ownership and local history contracts.
pub const PROTOCOL_VERSION: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeControlAction {
    Acquire,
    Release,
}

/// 클라이언트 → 서버.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// 첫 프레임 — 페어링 토큰 인증. `v`는 선택(구클라 호환).
    Auth {
        token: String,
        #[serde(default)]
        v: u32,
    },
    /// 승인 결정 — 기존 resolve_approval 경로로 직행한다.
    Resolve {
        id: String,
        allowed: bool,
        #[serde(default)]
        remember: bool,
    },
    /// 세션 시청 시작/전환 (터미널 뷰어 — P5b). 접속당 시청은 1개 — 새 watch가
    /// 이전 시청을 대체한다. 브리지가 refcount를 집계해 runtime lease로 승격한다.
    Watch { session: String },
    /// 시청 종료 — 접속은 유지한 채 시청만 끊는다 (WS 절단 시에는 자동 해제).
    Unwatch,
    /// 클라이언트 렌더 상태가 깨졌을 때 전체 화면 재동기화 요청 (P5c — remote.rs
    /// RequestKeyframe 관례). 서버는 baseline을 버려 다음 프레임을 keyframe으로 보낸다.
    RequestKeyframe,
    /// 시청 중 세션에 최소 제어 키 (P5d). 화이트리스트("ctrl_c"/"enter")만 서버가
    /// 바이트로 매핑한다 — 자유 타이핑·IME는 비범위(필요 시 별도 PR).
    Key { session: String, key: String },
    /// 시청 중 세션의 스크롤백 이동 (스크롤백 열람 — P5 후속). delta 양수 = 과거로.
    /// 스크롤 상태는 세션당 하나(데스크톱과 공유 — tmux 관례, RuntimeCommand::Scroll
    /// 재사용). 서버가 delta를 방어적으로 캡한다.
    Scroll { session: String, delta: i32 },
    /// 시청 중 세션에 자유 텍스트 입력 (P6a — composer). 서버가 C0 제어문자를 걷어내고
    /// (\t 제외 — 제어 시퀀스는 named key로만), \n을 \r로 정규화하며, 여러 줄/대형
    /// 텍스트는 세션의 bracketed paste 모드가 켜져 있으면 wrap한다. submit=true면
    /// 마지막에 Enter(\r)를 덧붙인다(전송), false면 삽입만(첨부 경로 등).
    Input {
        session: String,
        text: String,
        #[serde(default)]
        submit: bool,
    },
    /// Committed typing/IME text or explicit paste. Control bytes use DirectKey.
    DirectInput {
        session: String,
        text: String,
        #[serde(default)]
        paste: bool,
    },
    /// Named lowercase terminal key or one printable ASCII Ctrl/Alt key.
    DirectKey {
        session: String,
        key: String,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
        #[serde(default)]
        shift: bool,
        #[serde(default)]
        meta: bool,
    },
    /// 워크스페이스 전환 요청 (미러 진입 — I1b-2). 폰이 비활성 워크스페이스로 들어가
    /// 이어서 작업할 때 보낸다. 데스크탑 active를 그 워크스페이스로 전환시킨다(하드 미러 —
    /// single-source 브리지라 active가 바뀌면 폰·데스크탑이 같은 화면). 대기(warm)는 즉시
    /// 재사용, 절전은 워커 재생성+resume을 앱의 switch_workspace가 처리하고, 대기 워커 상한
    /// 초과는 앱이 거부하며 notice로 알린다. `workspace`는 프레임이 이미 폰에 준 워크스페이스
    /// id(안정 문자열 — 세션 u64와 달리 매핑 불필요)다.
    Switch { workspace: String },
    ResizeControl {
        session: String,
        action: ResizeControlAction,
        request: std::num::NonZeroU32,
    },
    Resize {
        session: String,
        request: std::num::NonZeroU32,
        cols: u16,
        rows: u16,
    },
}

impl ClientMsg {
    /// 텍스트 프레임을 파싱한다. 알 수 없는/기형 메시지는 None(무시).
    pub fn parse(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

/// 세션 한 행(대시보드). `status`는 snake_case 문자열(런타임 SessionStatus 매핑).
/// 제목은 앱이 해석한 표시명(프로젝트명 규칙 — 데스크톱 활동 패널과 동일)이다.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionView {
    /// 활성 워크스페이스 세션만 id가 있다 — 시청/입력 대상. 비활성(warm/유휴)은
    /// **표시 전용**: 세션 id는 worker-로컬이라 다른 워크스페이스 id로 시청하면
    /// 엉뚱한 세션이 잡힌다(P5 리뷰 P2에서 확인한 앨리어싱).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub title: String,
    /// 감지된 상태(런타임 이벤트 유래). warm/유휴는 상태 추적이 없어 생략된다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    /// 이 세션에서 돌고 있는 에이전트 요약("Claude · sonnet · high") — 앱의 감지
    /// 결과(agent_detect)다. 감지 워커는 활성 워크스페이스만 돌므로 warm/유휴는 없다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// 종료(SessionExited/Restored 관측) — 완료 배지용.
    pub exited: bool,
}

/// 워크스페이스 한 묶음(대시보드). 활성 1개 + warm/유휴 N개 — 데스크톱 활동 패널과
/// 같은 구성으로, 폰에서도 전체 워크스페이스가 보인다.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkspaceView {
    pub id: String,
    pub name: String,
    /// 연결 화면의 작업 경로. 셸은 textContent로만 렌더한다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_directory: Option<String>,
    /// "active" | "warm" | "suspended"
    pub state: &'static str,
    pub sessions: Vec<SessionView>,
}

/// 앱 프로세스 리소스 요약(대시보드). rss는 표시 편의상 MB.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceView {
    pub cpu: Option<f32>,
    pub rss_mb: u64,
}

/// 승인 대기 한 행. `preview`는 proxy가 이미 redact한 표시용 텍스트 — 클라이언트는
/// 반드시 textContent로만 삽입한다(innerHTML 금지, 불변 원칙 6/10).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApprovalView {
    pub id: String,
    pub server: String,
    pub tool: String,
    pub preview: String,
    pub created_at: i64,
    /// 이 승인을 요청한 세션의 **영속 UUID** — 폰의 "화면 보기"가 이걸로 watch한다
    /// (모듈 상단 규약: 폰은 worker-로컬 u64를 모른다). [`SessionView::id`]와 같은 공간.
    ///
    /// 채우는 경로: 승인 행의 `pane_id`(= 런타임 세션 키 `{ws}:{u64}`)를 파싱해 u64를
    /// 얻고, 브리지의 IdMap으로 UUID를 찾는다. **활성 워크스페이스의 승인만** 값이 있다 —
    /// IdMap은 활성 워커의 mux 스냅샷에서 오고, u64는 워크스페이스마다 1부터라 다른
    /// 워크스페이스 번호로 조회하면 엉뚱한 세션이 잡힌다.
    ///
    /// 2026-07-17 이전엔 DB 조인(`pane_id = mux_panes.id`)으로 채우려 했으나 두 값이 다른
    /// 식별자 공간이라 매칭된 적이 없어, 이 필드가 늘 None이었다(= 딥링크가 죽어 있었다).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// 세션 표시 제목 — 승인 카드에 "어느 세션인지"를 보인다. 세션 목록에 아직 없는
    /// 경우(막 뜬 세션 등)의 폴백이라, 폰은 목록에서 찾은 제목을 우선한다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
}

/// 커서 표시 상태 (P5c). shape는 "block"/"underline"/"beam".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CursorView {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    pub shape: &'static str,
}

/// 한 행 안의 스타일 run (P5c) — (fg, bg, wide)가 같은 연속 셀 묶음. `s`는 시작 셀
/// 열, `t`는 텍스트(wide_spacer 제외), `w`=true면 글자당 2셀 폭(한글 등).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunView {
    pub s: u16,
    pub t: String,
    /// Explicit owner-cell text; present only when scalar iteration would lose boundaries.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub g: Vec<String>,
    pub fg: String,
    pub bg: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub w: bool,
    /// SGR 속성 비트셋 (B-1) — 0이면 생략된다. 비트는 terminal::CellAttrs와 동일
    /// (1=bold, 2=italic, 4=underline, 8=strikeout, 16=dim). 폰 렌더러가 해석한다.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub a: u8,
}

fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

/// 화면 한 행 (P5c). delta 프레임에는 바뀐 행만 실린다.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LineView {
    pub row: u16,
    pub runs: Vec<RunView>,
}

/// 서버 → 클라이언트. 내부 태그(`type`)로 클라가 분기한다.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// 인증 성공 직후 1회.
    Welcome { v: u32 },
    /// 워크스페이스별 세션 목록 + 리소스 스냅샷. 활성 워크스페이스의 상태는 런타임
    /// 이벤트 유래(프레임 독립), 제목·비활성 워크스페이스는 앱 스냅샷 유래.
    Dashboard {
        workspaces: Vec<WorkspaceView>,
        resource: Option<ResourceView>,
        /// 일시 안내 배너(미러 진입 상한 초과 등 — I1b-2). 앱이 세팅하고 TTL 지나면
        /// 스스로 None으로 돌린다. 클라는 내용이 바뀔 때만 표시하고 몇 초 뒤 자동으로 숨긴다.
        #[serde(skip_serializing_if = "Option::is_none")]
        notice: Option<String>,
    },
    /// 승인 대기 목록(DB 폴링 유래).
    Approvals { pending: Vec<ApprovalView> },
    /// 시청 세션 화면 (P5c). keyframe=전체 행, delta=바뀐 행만(빈 lines면 커서만 갱신).
    /// 행 텍스트+스타일 run 인코딩 — 셀 단위 JSON 대비 수십 배 작다 (계획 §4 이식).
    Viewport {
        session: String,
        seq: u64,
        keyframe: bool,
        cols: u16,
        rows: u16,
        cursor: CursorView,
        alt: bool,
        /// 스크롤백 오프셋(줄) — 0 = 맨 아래(라이브). 클라가 "과거 열람 중" 표시와
        /// 맨 아래 복귀(delta = -offset)에 쓴다.
        offset: i32,
        lines: Vec<LineView>,
    },
    /// 시청 세션의 PTY 입력 큐 압박 (P6a) — composer 전송 버튼 게이트.
    /// queued=0이면 해소(재활성). reason: "queue_full"/"closed"/"too_large"/"unavailable".
    InputPressure {
        session: String,
        queued: usize,
        reason: &'static str,
    },
    /// 인증 실패 등 — 직후 close.
    Error { message: String },
    TerminalControl {
        session: String,
        request: u32,
        owned: bool,
        reason: &'static str,
    },
}

impl ServerMsg {
    /// JSON 텍스트로 직렬화한다. 직렬화 실패(사실상 불가)는 최소 error 프레임으로 대체.
    pub fn encode(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| r#"{"type":"error","message":"encode failed"}"#.to_owned())
    }
}

/// RGB → `#rrggbb` (JSON에서 배열보다 짧고 canvas fillStyle에 그대로 쓰인다).
fn hex_color(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

fn cursor_view(snapshot: &runtime::TerminalViewportSnapshot) -> CursorView {
    CursorView {
        col: snapshot.cursor.col,
        row: snapshot.cursor.row,
        visible: snapshot.cursor.visible,
        shape: match snapshot.cursor.shape {
            runtime::CursorShape::Block => "block",
            runtime::CursorShape::Underline => "underline",
            runtime::CursorShape::Beam => "beam",
        },
    }
}

/// 행 하나를 스타일 run들로 인코딩한다 (P5c). wide_spacer 셀은 건너뛰고(자리 채움 —
/// 렌더 안 함), (fg, bg, wide)가 같은 연속 셀을 하나의 run으로 합친다(사실상 행 RLE).
/// wide 전환에서도 run을 끊어 클라이언트가 run 단위 고정 폭(1 또는 2셀)으로 전진한다.
#[cfg(test)]
fn encode_line(cells: &[runtime::TerminalCell], row: u16) -> LineView {
    encode_line_with_graphemes(cells, row, &[])
}

fn encode_line_with_graphemes(
    cells: &[runtime::TerminalCell],
    row: u16,
    graphemes: &[runtime::CellGrapheme],
) -> LineView {
    let mut runs: Vec<RunView> = Vec::new();
    // 진행 중 run의 (fg, bg, wide, attrs, 다음 예상 열) — 셀마다 hex 문자열을 만들지 않는다.
    // 속성(B-1)이 다르면 run을 끊는다 — 폰도 bold/underline 등을 그린다.
    // 진행 중 run의 스타일 키 — clippy type_complexity 회피용 별칭.
    type OpenRun = ([u8; 3], [u8; 3], bool, u8, u16);
    let mut open: Option<OpenRun> = None;
    for (col, cell) in cells.iter().enumerate() {
        if cell.wide_spacer() {
            continue;
        }
        let col = col as u16;
        let advance = if cell.wide() { 2 } else { 1 };
        let attrs = cell.attrs().0;
        let index = row as usize * cells.len() + col as usize;
        if let Ok(entry) = graphemes.binary_search_by_key(&index, |entry| entry.index) {
            let text = &graphemes[entry].text;
            runs.push(RunView {
                s: col,
                t: text.clone(),
                g: vec![text.clone()],
                fg: hex_color(cell.fg),
                bg: hex_color(cell.bg),
                w: cell.wide(),
                a: attrs,
            });
            open = None;
            continue;
        }
        match (&mut open, runs.last_mut()) {
            (Some((fg, bg, wide, a, next)), Some(run))
                if *fg == cell.fg
                    && *bg == cell.bg
                    && *wide == cell.wide()
                    && *a == attrs
                    && *next == col =>
            {
                run.t.push(cell.c);
                *next = col + advance;
            }
            _ => {
                runs.push(RunView {
                    s: col,
                    t: cell.c.to_string(),
                    g: Vec::new(),
                    fg: hex_color(cell.fg),
                    bg: hex_color(cell.bg),
                    w: cell.wide(),
                    a: attrs,
                });
                open = Some((cell.fg, cell.bg, cell.wide(), attrs, col + advance));
            }
        }
    }
    LineView { row, runs }
}

/// 시청 화면 프레임을 만든다 (P5c). baseline이 없거나 화면 크기가 바뀌면 keyframe
/// (전체 행), 아니면 baseline과 셀이 다른 행만 담은 delta. 행 변화가 없어도 프레임은
/// 나간다 — 커서 이동만 있는 갱신을 클라이언트가 반영한다.
pub fn encode_viewport(
    session: &str,
    seq: u64,
    snapshot: &runtime::TerminalViewportSnapshot,
    baseline: Option<&runtime::TerminalViewportSnapshot>,
) -> ServerMsg {
    let cols = snapshot.cols as usize;
    let rows = snapshot.rows as usize;
    let keyframe = match baseline {
        Some(base) => base.cols != snapshot.cols || base.rows != snapshot.rows,
        None => true,
    };
    let mut lines = Vec::new();
    for row in 0..rows {
        let range = row * cols..(row + 1) * cols;
        let Some(cells) = snapshot.visible_cells.get(range.clone()) else {
            break; // 방어: cells 길이가 cols*rows보다 짧으면 있는 만큼만
        };
        let grapheme_changed =
            baseline.is_none_or(|base| base.row_graphemes(row) != snapshot.row_graphemes(row));
        let changed = if keyframe || grapheme_changed {
            true
        } else {
            // delta: baseline의 같은 행과 셀 비교 (keyframe이 아니면 기하는 동일)
            baseline
                .and_then(|base| base.visible_cells.get(range))
                .is_none_or(|base_cells| base_cells != cells)
        };
        if changed {
            lines.push(encode_line_with_graphemes(
                cells,
                row as u16,
                snapshot.row_graphemes(row),
            ));
        }
    }
    ServerMsg::Viewport {
        session: session.to_owned(),
        seq,
        keyframe,
        cols: snapshot.cols,
        rows: snapshot.rows,
        cursor: cursor_view(snapshot),
        alt: snapshot.is_alt_screen,
        offset: snapshot.scroll_offset,
        lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_control_wire_requires_nonzero_request_and_explicit_action() {
        for text in [
            r#"{"type":"resize_control","session":"uuid","action":"acquire","request":1}"#,
            r#"{"type":"resize_control","session":"uuid","action":"release","request":4294967295}"#,
            r#"{"type":"resize","session":"uuid","request":1,"cols":40,"rows":6}"#,
        ] {
            assert!(
                ClientMsg::parse(text).is_some(),
                "explicit correlated control wire: {text}"
            );
        }
        for text in [
            r#"{"type":"resize_control","session":"uuid","action":"acquire"}"#,
            r#"{"type":"resize_control","session":"uuid","action":"acquire","request":0}"#,
            r#"{"type":"resize_control","session":"uuid","action":"renew","request":1}"#,
            r#"{"type":"resize","session":"uuid","request":0,"cols":40,"rows":6}"#,
        ] {
            assert!(ClientMsg::parse(text).is_none());
        }
    }

    #[test]
    fn direct_terminal_input_wire_accepts_explicit_text_and_keys() {
        assert!(
            ClientMsg::parse(
                r#"{"type":"direct_input","session":"u7","text":"한글 ","paste":false}"#,
            )
            .is_some()
        );
        assert!(ClientMsg::parse(
            r#"{"type":"direct_key","session":"u7","key":"left","ctrl":true,"alt":false,"shift":false,"meta":false}"#,
        )
        .is_some());
    }

    #[test]
    fn auth_프레임을_파싱한다() {
        let msg = ClientMsg::parse(r#"{"type":"auth","v":1,"token":"abc"}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Auth {
                token: "abc".into(),
                v: 1
            }
        );
        // v 생략 허용(기본 0)
        let msg = ClientMsg::parse(r#"{"type":"auth","token":"x"}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Auth {
                token: "x".into(),
                v: 0
            }
        );
    }

    #[test]
    fn resolve_프레임을_파싱한다() {
        let msg = ClientMsg::parse(r#"{"type":"resolve","id":"i1","allowed":true}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Resolve {
                id: "i1".into(),
                allowed: true,
                remember: false
            }
        );
        let msg =
            ClientMsg::parse(r#"{"type":"resolve","id":"i2","allowed":false,"remember":true}"#)
                .unwrap();
        assert_eq!(
            msg,
            ClientMsg::Resolve {
                id: "i2".into(),
                allowed: false,
                remember: true
            }
        );
    }

    #[test]
    fn switch_프레임을_파싱한다() {
        // 미러 진입(I1b-2) — 워크스페이스 id만 실린다.
        let msg = ClientMsg::parse(r#"{"type":"switch","workspace":"ws-2"}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Switch {
                workspace: "ws-2".into()
            }
        );
        // workspace 누락은 파싱 실패(None) — 앱에 빈 전환이 가지 않는다.
        assert!(ClientMsg::parse(r#"{"type":"switch"}"#).is_none());
    }

    #[test]
    fn dashboard_notice는_some일때만_실린다() {
        let with = ServerMsg::Dashboard {
            workspaces: vec![],
            resource: None,
            notice: Some("대기 워커가 가득 찼습니다".into()),
        }
        .encode();
        assert!(
            with.contains(r#""notice":"대기 워커가 가득 찼습니다""#),
            "{with}"
        );
    }

    #[test]
    fn 기형이나_미지_메시지는_none() {
        assert!(ClientMsg::parse("not json").is_none());
        assert!(ClientMsg::parse(r#"{"type":"nope"}"#).is_none());
        assert!(ClientMsg::parse(r#"{"type":"auth"}"#).is_none()); // token 필수
    }

    #[test]
    fn watch_unwatch_프레임을_파싱한다() {
        let msg = ClientMsg::parse(r#"{"type":"watch","session":"u7"}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Watch {
                session: "u7".into()
            }
        );
        let msg = ClientMsg::parse(r#"{"type":"unwatch"}"#).unwrap();
        assert_eq!(msg, ClientMsg::Unwatch);
        // session 누락 watch는 기형 — 무시
        assert!(ClientMsg::parse(r#"{"type":"watch"}"#).is_none());
        let msg = ClientMsg::parse(r#"{"type":"request_keyframe"}"#).unwrap();
        assert_eq!(msg, ClientMsg::RequestKeyframe);
        let msg = ClientMsg::parse(r#"{"type":"key","session":"u7","key":"ctrl_c"}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Key {
                session: "u7".into(),
                key: "ctrl_c".into()
            }
        );
        let msg = ClientMsg::parse(r#"{"type":"scroll","session":"u7","delta":-12}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Scroll {
                session: "u7".into(),
                delta: -12
            }
        );
        // Input — submit 생략 시 false (삽입만)
        let msg = ClientMsg::parse(r#"{"type":"input","session":"u7","text":"ls"}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Input {
                session: "u7".into(),
                text: "ls".into(),
                submit: false
            }
        );
        let msg = ClientMsg::parse(r#"{"type":"input","session":"u7","text":"ls","submit":true}"#)
            .unwrap();
        assert_eq!(
            msg,
            ClientMsg::Input {
                session: "u7".into(),
                text: "ls".into(),
                submit: true
            }
        );
    }

    #[test]
    fn input_pressure_프레임_직렬화() {
        let json = ServerMsg::InputPressure {
            session: "u7".into(),
            queued: 4096,
            reason: "queue_full",
        }
        .encode();
        assert!(json.contains(r#""type":"input_pressure""#), "{json}");
        assert!(json.contains(r#""queued":4096"#), "{json}");
    }

    #[test]
    fn viewport_프레임은_스크롤백_오프셋을_싣는다() {
        let mut scrolled = snapshot(10, 2, vec![cell(' ', WHITE, BLACK); 20]);
        scrolled.scroll_offset = 42;
        let json = encode_viewport("u7", 1, &scrolled, None).encode();
        assert!(json.contains(r#""offset":42"#), "{json}");
    }

    // ── P5c 인코더 ──

    fn cell(c: char, fg: [u8; 3], bg: [u8; 3]) -> runtime::TerminalCell {
        runtime::TerminalCell::new(c, fg, bg, false, false, Default::default())
    }

    fn wide_pair(c: char, fg: [u8; 3], bg: [u8; 3]) -> [runtime::TerminalCell; 2] {
        [
            runtime::TerminalCell::new(c, fg, bg, true, false, Default::default()),
            runtime::TerminalCell::new(' ', fg, bg, false, true, Default::default()),
        ]
    }

    fn snapshot(
        cols: u16,
        rows: u16,
        cells: Vec<runtime::TerminalCell>,
    ) -> runtime::TerminalViewportSnapshot {
        assert_eq!(cells.len(), cols as usize * rows as usize);
        runtime::TerminalViewportSnapshot {
            cols,
            rows,
            cursor: runtime::CursorSnapshot {
                col: 0,
                row: 0,
                shape: runtime::CursorShape::Block,
                visible: true,
            },
            visible_cells: cells.into(),
            graphemes: Default::default(),
            dirty_ranges: Vec::new(),
            title: None,
            scroll_offset: 0,
            is_alt_screen: false,
        }
    }

    const WHITE: [u8; 3] = [255, 255, 255];
    const BLACK: [u8; 3] = [0, 0, 0];
    const RED: [u8; 3] = [255, 0, 0];

    #[test]
    fn sparse_grapheme_survives_web_keyframe_and_delta() {
        let base = snapshot(10, 3, vec![cell('a', WHITE, BLACK); 30]);
        let mut changed = base.clone();
        changed.graphemes = vec![runtime::CellGrapheme {
            index: 0,
            text: "a\u{301}\u{308}".into(),
        }]
        .into();
        for baseline in [None, Some(&base)] {
            let ServerMsg::Viewport { lines, .. } = encode_viewport("u7", 1, &changed, baseline)
            else {
                panic!("viewport")
            };
            assert!(!lines.is_empty(), "grapheme-only change must emit the row");
            assert!(lines[0].runs[0].t.starts_with("a\u{301}\u{308}"));
            assert_eq!(lines[0].runs[0].g, vec!["a\u{301}\u{308}"]);
            assert_eq!(lines[0].runs[1].s, 1);
        }
    }

    #[test]
    fn 행_run은_스타일과_폭_전환에서_끊고_spacer를_건너뛴다() {
        // "AB한글cd" — AB는 빨강, 한글은 wide, cd는 흰색
        let mut cells = vec![cell('A', RED, BLACK), cell('B', RED, BLACK)];
        cells.extend(wide_pair('한', WHITE, BLACK));
        cells.extend(wide_pair('글', WHITE, BLACK));
        cells.push(cell('c', WHITE, BLACK));
        cells.push(cell('d', WHITE, BLACK));
        let line = encode_line(&cells, 3);
        assert_eq!(line.row, 3);
        assert_eq!(line.runs.len(), 3, "{:?}", line.runs);
        assert_eq!((line.runs[0].s, line.runs[0].t.as_str()), (0, "AB"));
        assert_eq!(line.runs[0].fg, "#ff0000");
        assert!(!line.runs[0].w);
        // 한글 run: spacer를 건너뛰고 시작 열 2, 글자당 2셀
        assert_eq!((line.runs[1].s, line.runs[1].t.as_str()), (2, "한글"));
        assert!(line.runs[1].w);
        // wide 다음 ascii — 열 6부터
        assert_eq!((line.runs[2].s, line.runs[2].t.as_str()), (6, "cd"));
        assert!(!line.runs[2].w);
    }

    #[test]
    fn baseline_없으면_keyframe_있으면_바뀐_행만_delta() {
        let blank = snapshot(10, 3, vec![cell(' ', WHITE, BLACK); 30]);
        // keyframe: 전체 행
        let ServerMsg::Viewport {
            keyframe, lines, ..
        } = encode_viewport("u7", 1, &blank, None)
        else {
            panic!("viewport 아님")
        };
        assert!(keyframe);
        assert_eq!(lines.len(), 3);

        // delta: 1행만 변경 → 그 행만
        let mut changed_cells = vec![cell(' ', WHITE, BLACK); 30];
        changed_cells[10] = cell('x', WHITE, BLACK);
        let changed = snapshot(10, 3, changed_cells);
        let ServerMsg::Viewport {
            keyframe, lines, ..
        } = encode_viewport("u7", 2, &changed, Some(&blank))
        else {
            panic!("viewport 아님")
        };
        assert!(!keyframe);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].row, 1);

        // 화면 크기 변화 → keyframe 강제
        let resized = snapshot(10, 4, vec![cell(' ', WHITE, BLACK); 40]);
        let ServerMsg::Viewport { keyframe, .. } = encode_viewport("u7", 3, &resized, Some(&blank))
        else {
            panic!("viewport 아님")
        };
        assert!(keyframe, "cols/rows 변화는 keyframe이어야 함");
    }

    #[test]
    fn 커서만_바뀐_delta는_빈_lines로_커서를_나른다() {
        let base = snapshot(10, 2, vec![cell(' ', WHITE, BLACK); 20]);
        let mut moved = snapshot(10, 2, vec![cell(' ', WHITE, BLACK); 20]);
        moved.cursor.col = 5;
        let ServerMsg::Viewport {
            keyframe,
            lines,
            cursor,
            ..
        } = encode_viewport("u7", 2, &moved, Some(&base))
        else {
            panic!("viewport 아님")
        };
        assert!(!keyframe);
        assert!(lines.is_empty());
        assert_eq!(cursor.col, 5);
    }

    #[test]
    fn 행_첫_셀이_spacer면_건너뛰고_시작_열이_정확하다() {
        // 앞 행 wrap 잔재 등으로 행이 spacer로 시작하는 대칭 케이스 방어 (P5 리뷰 P3).
        let mut cells = vec![runtime::TerminalCell::new(
            ' ',
            WHITE,
            BLACK,
            false,
            true,
            Default::default(),
        )];
        cells.push(cell('a', WHITE, BLACK));
        cells.push(cell('b', WHITE, BLACK));
        let line = encode_line(&cells, 0);
        assert_eq!(line.runs.len(), 1, "{:?}", line.runs);
        assert_eq!((line.runs[0].s, line.runs[0].t.as_str()), (1, "ab"));
    }

    #[test]
    fn cells가_기하보다_짧으면_있는_행까지만_인코딩한다() {
        // 방어 경로: visible_cells 길이 < cols×rows — 패닉 없이 부분 keyframe (P5 리뷰 P3).
        let mut snapshot = snapshot(10, 3, vec![cell(' ', WHITE, BLACK); 30]);
        snapshot.visible_cells = vec![cell('x', WHITE, BLACK); 15].into(); // 1.5행분
        let ServerMsg::Viewport {
            keyframe, lines, ..
        } = encode_viewport("u7", 1, &snapshot, None)
        else {
            panic!("viewport 아님")
        };
        assert!(keyframe);
        assert_eq!(lines.len(), 1, "완전한 행(1개)까지만 실려야 함");
    }

    #[test]
    fn 프레임_크기_실측_80x24() {
        // 빈 화면 keyframe — 행당 run 1개
        let blank = snapshot(80, 24, vec![cell(' ', WHITE, BLACK); 80 * 24]);
        let blank_json = encode_viewport("u7", 1, &blank, None).encode();
        assert!(
            blank_json.len() < 8 * 1024,
            "빈 keyframe {}B ≥ 8KB",
            blank_json.len()
        );

        // 현실적 화면: 모든 행이 3색 run (프롬프트/출력/강조 혼합 가정)
        let mut cells = Vec::with_capacity(80 * 24);
        for _ in 0..24 {
            for col in 0..80u16 {
                let (fg, ch) = match col {
                    0..=9 => (RED, 'p'),
                    10..=59 => (WHITE, 'x'),
                    _ => ([0, 255, 0], ' '),
                };
                cells.push(cell(ch, fg, BLACK));
            }
        }
        let busy = snapshot(80, 24, cells);
        let busy_json = encode_viewport("u7", 1, &busy, None).encode();
        assert!(
            busy_json.len() < 16 * 1024,
            "3-run×24행 keyframe {}B ≥ 16KB (naive 셀 JSON은 ~75KB)",
            busy_json.len()
        );

        // 1행 delta
        let mut one_row = vec![cell(' ', WHITE, BLACK); 80 * 24];
        one_row[80] = cell('y', WHITE, BLACK);
        let delta_snapshot = snapshot(80, 24, one_row);
        let delta_json = encode_viewport("u7", 2, &delta_snapshot, Some(&blank)).encode();
        assert!(
            delta_json.len() < 1024,
            "1행 delta {}B ≥ 1KB",
            delta_json.len()
        );
        // 실측 기록 (--nocapture로 확인): naive 셀 JSON ~75KB 대비 수십 배 작다.
        eprintln!(
            "P5c 프레임 실측 — 빈 keyframe {}B, 3-run keyframe {}B, 1행 delta {}B",
            blank_json.len(),
            busy_json.len(),
            delta_json.len()
        );
    }

    #[test]
    fn 서버_프레임_직렬화는_type_태그를_싣는다() {
        let welcome = ServerMsg::Welcome {
            v: PROTOCOL_VERSION,
        }
        .encode();
        assert_eq!(
            welcome,
            format!(r#"{{"type":"welcome","v":{PROTOCOL_VERSION}}}"#)
        );

        let dash = ServerMsg::Dashboard {
            workspaces: vec![
                WorkspaceView {
                    id: "ws-1".into(),
                    name: "deppy-sijo".into(),
                    current_directory: Some("/Users/jr/deppy-sijo".into()),
                    state: "active",
                    sessions: vec![SessionView {
                        id: Some("u7".into()),
                        title: "claude".into(),
                        status: Some("needs_approval"),
                        agent: Some("Claude · sonnet · high".into()),
                        exited: false,
                    }],
                },
                WorkspaceView {
                    id: "ws-2".into(),
                    name: "source".into(),
                    current_directory: None,
                    state: "warm",
                    // 표시 전용 — id/status 없음(직렬화에서 생략된다)
                    sessions: vec![SessionView {
                        id: None,
                        title: "deppy-mux".into(),
                        status: None,
                        agent: None,
                        exited: false,
                    }],
                },
            ],
            resource: Some(ResourceView {
                cpu: Some(12.5),
                rss_mb: 340,
            }),
            notice: None,
        }
        .encode();
        assert!(dash.contains(r#""type":"dashboard""#), "{dash}");
        assert!(dash.contains(r#""status":"needs_approval""#), "{dash}");
        assert!(dash.contains(r#""rss_mb":340"#), "{dash}");
        assert!(dash.contains(r#""state":"warm""#), "{dash}");
        assert!(
            dash.contains(r#""current_directory":"/Users/jr/deppy-sijo""#),
            "{dash}"
        );
        // notice None이면 프레임에 실리지 않는다(skip_serializing_if).
        assert!(!dash.contains(r#""notice""#), "{dash}");
        assert!(
            dash.contains(r#""agent":"Claude · sonnet · high""#),
            "{dash}"
        );
        // 비활성 세션은 id/status가 아예 실리지 않는다(클라가 "보기" 버튼을 안 만든다)
        let warm_part = dash.split(r#""name":"source""#).nth(1).unwrap();
        assert!(!warm_part.contains(r#""id":"#), "{dash}");
        assert!(!warm_part.contains(r#""status":"#), "{dash}");

        let appr = ServerMsg::Approvals {
            pending: vec![ApprovalView {
                id: "a1".into(),
                server: "github".into(),
                tool: "create_issue".into(),
                preview: "{redacted}".into(),
                created_at: 1720,
                session: None,
                session_title: None,
            }],
        }
        .encode();
        assert!(appr.contains(r#""type":"approvals""#), "{appr}");
        assert!(appr.contains(r#""tool":"create_issue""#), "{appr}");
    }
}
