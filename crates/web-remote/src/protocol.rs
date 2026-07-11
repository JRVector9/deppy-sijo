//! WS JSON 프로토콜 v1 — 브라우저 친화 텍스트 프레임 (계획 v3.3 P2).
//!
//! postcard 바이너리 코덱은 데스크톱 원격(remote.rs) 전용으로 남기고, 폰 PWA는 JSON만
//! 쓴다. 첫 프레임은 반드시 `{"type":"auth", ...}` — 인증 전 다른 메시지는 무시된다.
//!
//! 서버→클라 프레임은 대시보드(런타임 유래)와 승인(DB 유래)이 갱신 주기가 달라 분리해
//! 보낸다. 클라이언트는 각 프레임을 독립적으로 반영한다.

use serde::{Deserialize, Serialize};

/// 프로토콜 버전 — 클라/서버 합의값. 하위호환이 깨지면 증가시킨다.
pub const PROTOCOL_VERSION: u32 = 1;

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
    Watch { session: u64 },
    /// 시청 종료 — 접속은 유지한 채 시청만 끊는다 (WS 절단 시에는 자동 해제).
    Unwatch,
    /// 클라이언트 렌더 상태가 깨졌을 때 전체 화면 재동기화 요청 (P5c — remote.rs
    /// RequestKeyframe 관례). 서버는 baseline을 버려 다음 프레임을 keyframe으로 보낸다.
    RequestKeyframe,
    /// 시청 중 세션에 최소 제어 키 (P5d). 화이트리스트("ctrl_c"/"enter")만 서버가
    /// 바이트로 매핑한다 — 자유 타이핑·IME는 비범위(필요 시 별도 PR).
    Key { session: u64, key: String },
    /// 시청 중 세션의 스크롤백 이동 (스크롤백 열람 — P5 후속). delta 양수 = 과거로.
    /// 스크롤 상태는 세션당 하나(데스크톱과 공유 — tmux 관례, RuntimeCommand::Scroll
    /// 재사용). 서버가 delta를 방어적으로 캡한다.
    Scroll { session: u64, delta: i32 },
    /// 시청 중 세션에 자유 텍스트 입력 (P6a — composer). 서버가 C0 제어문자를 걷어내고
    /// (\t 제외 — 제어 시퀀스는 named key로만), \n을 \r로 정규화하며, 여러 줄/대형
    /// 텍스트는 세션의 bracketed paste 모드가 켜져 있으면 wrap한다. submit=true면
    /// 마지막에 Enter(\r)를 덧붙인다(전송), false면 삽입만(첨부 경로 등).
    Input {
        session: u64,
        text: String,
        #[serde(default)]
        submit: bool,
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
    pub id: Option<u64>,
    pub title: String,
    /// 감지된 상태(런타임 이벤트 유래). warm/유휴는 상태 추적이 없어 생략된다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    /// 종료(SessionExited/Restored 관측) — 완료 배지용.
    pub exited: bool,
}

/// 워크스페이스 한 묶음(대시보드). 활성 1개 + warm/유휴 N개 — 데스크톱 활동 패널과
/// 같은 구성으로, 폰에서도 전체 워크스페이스가 보인다.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkspaceView {
    pub id: String,
    pub name: String,
    /// "active" | "warm" | "idle"
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
    pub fg: String,
    pub bg: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub w: bool,
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
    },
    /// 승인 대기 목록(DB 폴링 유래).
    Approvals { pending: Vec<ApprovalView> },
    /// 시청 세션 화면 (P5c). keyframe=전체 행, delta=바뀐 행만(빈 lines면 커서만 갱신).
    /// 행 텍스트+스타일 run 인코딩 — 셀 단위 JSON 대비 수십 배 작다 (계획 §4 이식).
    Viewport {
        session: u64,
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
        session: u64,
        queued: usize,
        reason: &'static str,
    },
    /// 인증 실패 등 — 직후 close.
    Error { message: String },
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
fn encode_line(cells: &[runtime::TerminalCell], row: u16) -> LineView {
    let mut runs: Vec<RunView> = Vec::new();
    // 진행 중 run의 (fg, bg, wide, 다음 예상 열) — 셀마다 hex 문자열을 만들지 않는다.
    let mut open: Option<([u8; 3], [u8; 3], bool, u16)> = None;
    for (col, cell) in cells.iter().enumerate() {
        if cell.wide_spacer {
            continue;
        }
        let col = col as u16;
        let advance = if cell.wide { 2 } else { 1 };
        match (&mut open, runs.last_mut()) {
            (Some((fg, bg, wide, next)), Some(run))
                if *fg == cell.fg && *bg == cell.bg && *wide == cell.wide && *next == col =>
            {
                run.t.push(cell.c);
                *next = col + advance;
            }
            _ => {
                runs.push(RunView {
                    s: col,
                    t: cell.c.to_string(),
                    fg: hex_color(cell.fg),
                    bg: hex_color(cell.bg),
                    w: cell.wide,
                });
                open = Some((cell.fg, cell.bg, cell.wide, col + advance));
            }
        }
    }
    LineView { row, runs }
}

/// 시청 화면 프레임을 만든다 (P5c). baseline이 없거나 화면 크기가 바뀌면 keyframe
/// (전체 행), 아니면 baseline과 셀이 다른 행만 담은 delta. 행 변화가 없어도 프레임은
/// 나간다 — 커서 이동만 있는 갱신을 클라이언트가 반영한다.
pub fn encode_viewport(
    session: u64,
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
        let changed = if keyframe {
            true
        } else {
            // delta: baseline의 같은 행과 셀 비교 (keyframe이 아니면 기하는 동일)
            baseline
                .and_then(|base| base.visible_cells.get(range))
                .is_none_or(|base_cells| base_cells != cells)
        };
        if changed {
            lines.push(encode_line(cells, row as u16));
        }
    }
    ServerMsg::Viewport {
        session,
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
    fn 기형이나_미지_메시지는_none() {
        assert!(ClientMsg::parse("not json").is_none());
        assert!(ClientMsg::parse(r#"{"type":"nope"}"#).is_none());
        assert!(ClientMsg::parse(r#"{"type":"auth"}"#).is_none()); // token 필수
    }

    #[test]
    fn watch_unwatch_프레임을_파싱한다() {
        let msg = ClientMsg::parse(r#"{"type":"watch","session":7}"#).unwrap();
        assert_eq!(msg, ClientMsg::Watch { session: 7 });
        let msg = ClientMsg::parse(r#"{"type":"unwatch"}"#).unwrap();
        assert_eq!(msg, ClientMsg::Unwatch);
        // session 누락 watch는 기형 — 무시
        assert!(ClientMsg::parse(r#"{"type":"watch"}"#).is_none());
        let msg = ClientMsg::parse(r#"{"type":"request_keyframe"}"#).unwrap();
        assert_eq!(msg, ClientMsg::RequestKeyframe);
        let msg = ClientMsg::parse(r#"{"type":"key","session":7,"key":"ctrl_c"}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Key {
                session: 7,
                key: "ctrl_c".into()
            }
        );
        let msg = ClientMsg::parse(r#"{"type":"scroll","session":7,"delta":-12}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Scroll {
                session: 7,
                delta: -12
            }
        );
        // Input — submit 생략 시 false (삽입만)
        let msg = ClientMsg::parse(r#"{"type":"input","session":7,"text":"ls"}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Input {
                session: 7,
                text: "ls".into(),
                submit: false
            }
        );
        let msg =
            ClientMsg::parse(r#"{"type":"input","session":7,"text":"ls","submit":true}"#).unwrap();
        assert_eq!(
            msg,
            ClientMsg::Input {
                session: 7,
                text: "ls".into(),
                submit: true
            }
        );
    }

    #[test]
    fn input_pressure_프레임_직렬화() {
        let json = ServerMsg::InputPressure {
            session: 7,
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
        let json = encode_viewport(7, 1, &scrolled, None).encode();
        assert!(json.contains(r#""offset":42"#), "{json}");
    }

    // ── P5c 인코더 ──

    fn cell(c: char, fg: [u8; 3], bg: [u8; 3]) -> runtime::TerminalCell {
        runtime::TerminalCell {
            c,
            fg,
            bg,
            wide: false,
            wide_spacer: false,
        }
    }

    fn wide_pair(c: char, fg: [u8; 3], bg: [u8; 3]) -> [runtime::TerminalCell; 2] {
        [
            runtime::TerminalCell {
                c,
                fg,
                bg,
                wide: true,
                wide_spacer: false,
            },
            runtime::TerminalCell {
                c: ' ',
                fg,
                bg,
                wide: false,
                wide_spacer: true,
            },
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
        } = encode_viewport(7, 1, &blank, None)
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
        } = encode_viewport(7, 2, &changed, Some(&blank))
        else {
            panic!("viewport 아님")
        };
        assert!(!keyframe);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].row, 1);

        // 화면 크기 변화 → keyframe 강제
        let resized = snapshot(10, 4, vec![cell(' ', WHITE, BLACK); 40]);
        let ServerMsg::Viewport { keyframe, .. } = encode_viewport(7, 3, &resized, Some(&blank))
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
        } = encode_viewport(7, 2, &moved, Some(&base))
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
        let mut cells = vec![runtime::TerminalCell {
            c: ' ',
            fg: WHITE,
            bg: BLACK,
            wide: false,
            wide_spacer: true,
        }];
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
        } = encode_viewport(7, 1, &snapshot, None)
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
        let blank_json = encode_viewport(7, 1, &blank, None).encode();
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
        let busy_json = encode_viewport(7, 1, &busy, None).encode();
        assert!(
            busy_json.len() < 16 * 1024,
            "3-run×24행 keyframe {}B ≥ 16KB (naive 셀 JSON은 ~75KB)",
            busy_json.len()
        );

        // 1행 delta
        let mut one_row = vec![cell(' ', WHITE, BLACK); 80 * 24];
        one_row[80] = cell('y', WHITE, BLACK);
        let delta_snapshot = snapshot(80, 24, one_row);
        let delta_json = encode_viewport(7, 2, &delta_snapshot, Some(&blank)).encode();
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
        assert_eq!(welcome, r#"{"type":"welcome","v":1}"#);

        let dash = ServerMsg::Dashboard {
            workspaces: vec![
                WorkspaceView {
                    id: "ws-1".into(),
                    name: "deppy-sijo".into(),
                    state: "active",
                    sessions: vec![SessionView {
                        id: Some(7),
                        title: "claude".into(),
                        status: Some("needs_approval"),
                        exited: false,
                    }],
                },
                WorkspaceView {
                    id: "ws-2".into(),
                    name: "source".into(),
                    state: "warm",
                    // 표시 전용 — id/status 없음(직렬화에서 생략된다)
                    sessions: vec![SessionView {
                        id: None,
                        title: "deppy-mux".into(),
                        status: None,
                        exited: false,
                    }],
                },
            ],
            resource: Some(ResourceView {
                cpu: Some(12.5),
                rss_mb: 340,
            }),
        }
        .encode();
        assert!(dash.contains(r#""type":"dashboard""#), "{dash}");
        assert!(dash.contains(r#""status":"needs_approval""#), "{dash}");
        assert!(dash.contains(r#""rss_mb":340"#), "{dash}");
        assert!(dash.contains(r#""state":"warm""#), "{dash}");
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
            }],
        }
        .encode();
        assert!(appr.contains(r#""type":"approvals""#), "{appr}");
        assert!(appr.contains(r#""tool":"create_issue""#), "{appr}");
    }
}
