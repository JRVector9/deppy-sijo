//! 벨 팝오버 「대기 중」 섹션의 PTY 입력 대기 카드 (v3.9 N3).
//!
//! hook이 보고한 needsInput(claude/codex의 y/n·메뉴 번호 선택 프롬프트)에 **그
//! 워크스페이스/세션으로 이동하지 않고** 응답한다. 이 파일은 렌더 + 미리보기 캐시 +
//! 순수 파싱/포맷 함수만 담당한다. 카드 데이터 조립(활성/warm 워크스페이스의 여러
//! 필드에서 제목·미리보기·UUID를 모으는 일)과 실제 명령 전송은 app.rs가 한다 —
//! 이 모듈은 leaf UI 경계(xtask check-boundary)를 지켜 DB/런타임 구체 타입을 직접
//! 참조하지 않는다.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use runtime::{MuxSnapshot, SessionId};

/// 로그 tail 읽기 상한 — 4KB만 읽는다(계획서 §PR-N3, UI 스레드 블로킹 방지).
const PREVIEW_TAIL_BYTES: u64 = 4096;
/// 미리보기 줄 수 — 1차는 상수로 고정한다(2026-07-17 확정 사항, 설정화하지 않음).
const PREVIEW_LINES: usize = 3;
/// 미리보기 캐시 TTL — ui/workspace.rs의 hover_cwd(PATH_CACHE_TTL=2s)와 같은
/// stale-while-revalidate 관례. tail은 lsof보다 무거운 파일 IO라 조금 더 넉넉히 둔다.
const PREVIEW_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(4);

/// 카드 하나의 표시 데이터. app.rs가 활성/warm 워크스페이스의 여러 필드(session_titles,
/// mux 스냅샷, 메모리 summary 또는 로그 tail)에서 조립해 넘긴다.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitingCard {
    pub workspace_id: String,
    pub session: SessionId,
    pub workspace_name: String,
    pub session_title: String,
    /// 화면 마지막 N줄(비어있지 않은 줄만). 조회 실패/미지원이면 None — 미리보기만
    /// 비고 카드(버튼)는 정상 동작한다(계획서 §PR-N3 폴백 규약).
    pub preview: Option<Vec<String>>,
}

/// 카드에서 사용자가 취한 액션. app.rs가 소비한다.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitingAction {
    /// [y]/[n]/자유 입력 — 문자열(개행은 app.rs가 붙인다)을 그 세션에 주입해 달라는 요청.
    /// stale 재확인·실제 전송은 app.rs 몫이다(이 모듈은 런타임 클라이언트를 모른다).
    Answer {
        workspace_id: String,
        session: SessionId,
        reply: String,
    },
    /// [이동→] — 기존 알림 네비게이션 파이프라인(plan_agent_notification_navigation)에
    /// 그대로 태울 수 있게 AgentNotificationTarget::Pty를 실어 돌려준다.
    Goto(super::notifications::AgentNotificationTarget),
}

/// 세션 로그 tail 캐시 항목.
struct TailCacheEntry {
    lines: Option<Vec<String>>,
    fetched_at: std::time::Instant,
    /// 백그라운드 재조회 진행 중 — 중복 spawn 방지 (hover_cwd의 CwdCacheEntry와 동일 역할).
    inflight: bool,
}

/// 팝오버 PTY 카드의 렌더 상태 — 자유 입력칸 버퍼 + 로그 tail 미리보기 캐시.
/// App이 소유하고, 팝오버가 열려 있을 때만 메서드를 호출한다(idle 비용 0 — 호출측 게이트).
pub struct InboxWaitingUi {
    /// 세션 로그 tail 캐시 (key: 영속 세션 UUID = PaneSnapshot::persistent_session_id).
    /// hover_cwd(ui/workspace.rs)와 같은 stale-while-revalidate — 백그라운드 스레드
    /// 1회 조회 + TTL, 그동안 stale 값(있으면)을 즉시 반환한다.
    tail_cache: Arc<Mutex<HashMap<String, TailCacheEntry>>>,
    /// 카드별 자유 입력 버퍼 — (workspace_id, session)으로 프레임 간 유지한다.
    inputs: HashMap<(String, SessionId), String>,
}

impl InboxWaitingUi {
    pub fn new() -> Self {
        Self {
            tail_cache: Arc::new(Mutex::new(HashMap::new())),
            inputs: HashMap::new(),
        }
    }

    /// 세션 로그 tail 미리보기. 캐시가 신선하거나 조회 진행 중이면 그 값을 즉시 반환하고,
    /// 만료/부재면 백그라운드 스레드로 재조회를 걸고 stale 값(있으면)을 즉시 돌려준다.
    /// 유효하지 않은 키·읽기 실패는 None — 호출측이 미리보기를 생략한다(카드는 정상 동작).
    pub fn preview(
        &mut self,
        ctx: &egui::Context,
        logs_root: &Path,
        session_uuid: &str,
    ) -> Option<Vec<String>> {
        let mut cache = self.tail_cache.lock().expect("tail cache lock");
        if let Some(entry) = cache.get(session_uuid)
            && (entry.inflight || entry.fetched_at.elapsed() < PREVIEW_CACHE_TTL)
        {
            return entry.lines.clone();
        }
        let stale = cache
            .get(session_uuid)
            .and_then(|entry| entry.lines.clone());
        // session_dir_key는 경로 탈출을 막는 방어적 검증도 겸한다(storage::logs 계약).
        // 실패(비정상 UUID)는 캐시에 실패로 기록해 매 프레임 재시도하지 않는다.
        let Ok(path) = storage::SessionLogWriter::session_dir_key(logs_root, session_uuid)
            .map(|dir| dir.join("redacted.plain.txt"))
        else {
            cache.insert(
                session_uuid.to_owned(),
                TailCacheEntry {
                    lines: None,
                    fetched_at: std::time::Instant::now(),
                    inflight: false,
                },
            );
            return None;
        };
        cache.insert(
            session_uuid.to_owned(),
            TailCacheEntry {
                lines: stale.clone(),
                fetched_at: std::time::Instant::now(),
                inflight: true,
            },
        );
        drop(cache);
        let shared = Arc::clone(&self.tail_cache);
        let ctx = ctx.clone();
        let key = session_uuid.to_owned();
        std::thread::Builder::new()
            .name("inbox-tail".to_owned())
            .spawn(move || {
                let lines = read_tail_lines(&path, PREVIEW_TAIL_BYTES, PREVIEW_LINES);
                shared.lock().expect("tail cache lock").insert(
                    key,
                    TailCacheEntry {
                        lines,
                        fetched_at: std::time::Instant::now(),
                        inflight: false,
                    },
                );
                // 마우스가 정지 상태면 자연 repaint가 없다 — 결과 도착을 명시 요청
                // (hover_cwd와 동일 관례).
                ctx.request_repaint();
            })
            .expect("tail read thread spawn");
        stale
    }

    /// 팝오버 「대기 중」 섹션의 PTY 카드들을 그린다. 카드가 비어 있으면 아무것도
    /// 그리지 않는다(빈 섹션 헤더를 보이지 않는다 — MCP 승인 카드만 있을 수 있다).
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        cards: &[WaitingCard],
    ) -> Option<WaitingAction> {
        if cards.is_empty() {
            return None;
        }
        super::notifications::section_label(ui, &catalog.t("inbox.waiting.section", &[]));
        let mut action = None;
        for card in cards {
            ui.add_space(4.0);
            self.render_card(ui, catalog, card, &mut action);
        }
        action
    }

    fn render_card(
        &mut self,
        ui: &mut egui::Ui,
        catalog: &i18n::Catalog,
        card: &WaitingCard,
        action: &mut Option<WaitingAction>,
    ) {
        ui.horizontal(|ui| {
            ui.label("⏳");
            ui.label(
                egui::RichText::new(format!("{} · {}", card.workspace_name, card.session_title))
                    .size(12.0)
                    .strong(),
            );
        });
        render_preview(ui, catalog, card.preview.as_deref());
        ui.horizontal(|ui| {
            if ui.small_button("y").clicked() {
                *action = Some(WaitingAction::Answer {
                    workspace_id: card.workspace_id.clone(),
                    session: card.session,
                    reply: "y".to_owned(),
                });
            }
            if ui.small_button("n").clicked() {
                *action = Some(WaitingAction::Answer {
                    workspace_id: card.workspace_id.clone(),
                    session: card.session,
                    reply: "n".to_owned(),
                });
            }
            let input_key = (card.workspace_id.clone(), card.session);
            let buf = self.inputs.entry(input_key.clone()).or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(buf)
                    .desired_width(72.0)
                    .hint_text(catalog.t("inbox.waiting.answer_hint", &[])),
            );
            let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if submit {
                let reply = buf.clone();
                self.inputs.remove(&input_key);
                *action = Some(WaitingAction::Answer {
                    workspace_id: card.workspace_id.clone(),
                    session: card.session,
                    reply,
                });
            }
            if ui
                .small_button(catalog.t("inbox.waiting.goto", &[]))
                .clicked()
            {
                *action = Some(WaitingAction::Goto(
                    super::notifications::AgentNotificationTarget::Pty {
                        workspace_id: card.workspace_id.clone(),
                        session: card.session,
                    },
                ));
            }
        });
    }
}

/// 미리보기 3줄 — 고정폭, 좌측 세로선(section_label과 같은 스타일의 accent bar).
/// 없으면 폴백 문구 하나만 보인다.
fn render_preview(ui: &mut egui::Ui, catalog: &i18n::Catalog, preview: Option<&[String]>) {
    let Some(lines) = preview.filter(|lines| !lines.is_empty()) else {
        ui.label(
            egui::RichText::new(catalog.t("inbox.waiting.no_preview", &[]))
                .size(10.5)
                .weak(),
        );
        return;
    };
    ui.horizontal(|ui| {
        let row_h = 13.0;
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(2.0, row_h * lines.len() as f32),
            egui::Sense::hover(),
        );
        ui.painter().rect_filled(
            rect,
            1.0,
            ui.visuals().widgets.noninteractive.bg_stroke.color,
        );
        ui.vertical(|ui| {
            for line in lines {
                ui.label(egui::RichText::new(line).monospace().size(10.0).weak());
            }
        });
    });
}

/// 라이브 mux 스냅샷에서 세션의 영속 UUID(sessions.id)를 찾는다. PaneSnapshot에 이미
/// 실려 온다(v3.7 I1: "경계를 넘는 식별자는 이 UUID를 써야" — 알림 딥링크와 같은 계약이라
/// DB 조인 없이 얻을 수 있다). warm 워크스페이스는 mux 스냅샷이 최신이 아닐 수 있어
/// (§14.1 — Warm은 렌더 경로로만 갱신된다) 못 찾을 수 있다: 그 경우 호출측이 미리보기를
/// 생략한다(카드 자체는 정상 동작 — 계획서 §PR-N3 폴백 규약).
pub fn find_persistent_session_id(mux: &MuxSnapshot, session: SessionId) -> Option<String> {
    mux.tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .find(|pane| pane.session_id == Some(session))
        .and_then(|pane| pane.persistent_session_id.clone())
}

/// hook 세션 키(`{workspace_id}:{u64}`) 파싱 — runtime::in_process의 session_key와 같은
/// 규약(SessionId가 워커마다 1부터 재배정되므로 workspace_id로 스코프한다, v3.7 codex High).
pub fn parse_session_key(key: &str) -> Option<(String, SessionId)> {
    let (workspace_id, session_id) = key.rsplit_once(':')?;
    if workspace_id.is_empty() {
        return None;
    }
    let session_id = session_id.parse::<u64>().ok()?;
    Some((workspace_id.to_owned(), SessionId(session_id)))
}

/// 텍스트의 마지막 n줄(비어있지 않은 줄만, 원래 순서 유지) — 로그 tail 미리보기 추출.
/// 순수 함수(유닛 테스트 대상) — 파일 IO는 read_tail_lines가 감싼다.
fn last_lines(text: &str, n: usize) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(n)
        .map(str::to_owned)
        .collect();
    lines.reverse();
    lines
}

/// 파일 끝에서 최대 `max_bytes`만 읽어 마지막 `n`줄을 추출한다(백그라운드 스레드 전용 —
/// UI 스레드에서 호출 금지). 파일 없음/읽기 실패/빈 결과는 None — 호출측이 폴백한다.
fn read_tail_lines(path: &Path, max_bytes: u64, n: usize) -> Option<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let lines = last_lines(&text, n);
    (!lines.is_empty()).then_some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{LayoutNode, MuxPaneId, MuxTabId, PaneSnapshot, TabSnapshot};

    #[test]
    fn parse_session_key_parses_workspace_and_session() {
        assert_eq!(
            parse_session_key("ws-abc:42"),
            Some(("ws-abc".to_owned(), SessionId(42)))
        );
    }

    #[test]
    fn parse_session_key_rejects_malformed_input() {
        assert_eq!(parse_session_key("no-colon"), None);
        assert_eq!(parse_session_key("ws:not-a-number"), None);
        assert_eq!(parse_session_key(":42"), None); // 빈 workspace id
        assert_eq!(parse_session_key("ws:"), None); // 빈 session id
    }

    #[test]
    fn last_lines_returns_last_n_non_empty_in_order() {
        let text = "a\nb\n\nc\nd\ne\n";
        assert_eq!(
            last_lines(text, 3),
            vec!["c".to_owned(), "d".to_owned(), "e".to_owned()]
        );
    }

    #[test]
    fn last_lines_returns_all_when_fewer_than_n() {
        assert_eq!(last_lines("only\n", 3), vec!["only".to_owned()]);
    }

    #[test]
    fn last_lines_empty_or_blank_text_returns_empty() {
        assert!(last_lines("", 3).is_empty());
        assert!(last_lines("\n\n\n", 3).is_empty());
    }

    fn snapshot_with_pane(pane_id: &str, session: SessionId, uuid: Option<&str>) -> MuxSnapshot {
        MuxSnapshot {
            tabs: vec![TabSnapshot {
                id: MuxTabId("t1".to_owned()),
                title: "tab".to_owned(),
                layout: LayoutNode::Pane(MuxPaneId(pane_id.to_owned())),
                panes: vec![PaneSnapshot {
                    id: MuxPaneId(pane_id.to_owned()),
                    session_id: Some(session),
                    title: "shell".to_owned(),
                    persistent_session_id: uuid.map(str::to_owned),
                }],
            }],
            active_tab: Some(MuxTabId("t1".to_owned())),
            focused_pane: None,
        }
    }

    #[test]
    fn find_persistent_session_id_matches_by_live_session() {
        let mux = snapshot_with_pane("p1", SessionId(1), Some("uuid-1"));
        assert_eq!(
            find_persistent_session_id(&mux, SessionId(1)),
            Some("uuid-1".to_owned())
        );
        assert_eq!(find_persistent_session_id(&mux, SessionId(2)), None);
    }

    #[test]
    fn find_persistent_session_id_none_when_pane_has_no_persisted_uuid() {
        let mux = snapshot_with_pane("p1", SessionId(1), None);
        assert_eq!(find_persistent_session_id(&mux, SessionId(1)), None);
    }

    /// hover_cwd(ui/workspace.rs) 테스트와 같은 stale-while-revalidate 검증 패턴:
    /// 1차 호출은 pending(None)이고, 백그라운드 스레드가 끝나면 캐시된 값이 온다.
    #[test]
    fn preview_reads_tail_via_background_thread_and_caches() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-inbox-waiting-test-{}-{}",
            std::process::id(),
            "preview_reads_tail"
        ));
        let session_uuid = "test-session-uuid";
        let log_dir = dir.join(session_uuid);
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("redacted.plain.txt"),
            "line1\nline2\nline3\nline4\n",
        )
        .unwrap();

        let mut ui = InboxWaitingUi::new();
        let ctx = egui::Context::default();
        let first = ui.preview(&ctx, &dir, session_uuid);
        assert!(first.is_none(), "1차 호출은 아직 조회 전이라 None이어야 함");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(lines) = ui.preview(&ctx, &dir, session_uuid) {
                assert_eq!(
                    lines,
                    vec!["line2".to_owned(), "line3".to_owned(), "line4".to_owned()]
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "미리보기 결과가 오지 않음"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preview_missing_file_resolves_to_none_without_retry_storm() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-inbox-waiting-test-{}-{}",
            std::process::id(),
            "preview_missing"
        ));
        let mut ui = InboxWaitingUi::new();
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let result = ui.preview(&ctx, &dir, "no-such-session");
            if result.is_none() {
                // inflight든 완료든, 파일이 없으니 결국 None으로 안정된다.
                if !ui
                    .tail_cache
                    .lock()
                    .unwrap()
                    .get("no-such-session")
                    .is_some_and(|entry| entry.inflight)
                {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "캐시가 안정화되지 않음"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    // ── kittest 상호작용 테스트 (2026-07-17) ──
    // "그 세션에 가지 않고 y/n/번호로 응답"의 UI 절반을 실제 클릭·타이핑 시뮬레이션으로
    // 자동 검증한다(주입 절반 WriteInput은 runtime 테스트가 커버).

    fn card(session: u64) -> WaitingCard {
        WaitingCard {
            workspace_id: "ws-1".to_owned(),
            session: SessionId(session),
            workspace_name: "proj".to_owned(),
            session_title: "codex".to_owned(),
            preview: None,
        }
    }

    /// 액션들을 프레임 너머로 수집하는 하네스. InboxWaitingUi(입력 버퍼)도 상태에 넣어
    /// 프레임 간 유지한다 — 실제 App과 동일한 수명.
    fn waiting_harness<'a>(
        catalog: &'a i18n::Catalog,
        cards: &'a [WaitingCard],
    ) -> egui_kittest::Harness<'a, (InboxWaitingUi, Vec<WaitingAction>)> {
        egui_kittest::Harness::new_ui_state(
            move |ui, (widget, captured): &mut (InboxWaitingUi, Vec<WaitingAction>)| {
                if let Some(action) = widget.render(ui, catalog, cards) {
                    captured.push(action);
                }
            },
            (InboxWaitingUi::new(), Vec::new()),
        )
    }

    #[test]
    fn kittest_y_클릭이_answer_y를_만든다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let cards = vec![card(7)];
        let mut harness = waiting_harness(&catalog, &cards);
        harness.get_by_label("y").click();
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![WaitingAction::Answer {
                workspace_id: "ws-1".to_owned(),
                session: SessionId(7),
                reply: "y".to_owned(),
            }]
        );
    }

    #[test]
    fn kittest_자유입력_타이핑_후_enter가_answer를_만들고_버퍼를_비운다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let cards = vec![card(7)];
        let mut harness = waiting_harness(&catalog, &cards);
        // 번호 응답("2↵") 시나리오 — claude/codex 메뉴 선택. 실제 사용자처럼 입력칸을
        // 먼저 클릭(포커스)한다 — kittest type_text는 Event::Text만 넣으므로 포커스가
        // 없으면 TextEdit이 무시한다.
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .click();
        harness.run();
        harness
            .get_by_role(egui::accesskit::Role::TextInput)
            .type_text("2");
        harness.run();
        harness.key_combination(&[egui::Key::Enter]);
        harness.run();
        let answers = &harness.state().1;
        assert_eq!(
            answers,
            &vec![WaitingAction::Answer {
                workspace_id: "ws-1".to_owned(),
                session: SessionId(7),
                reply: "2".to_owned(),
            }],
            "Enter 시 입력 내용이 Answer로 나와야 한다"
        );
        // 제출 시 항목이 remove되지만 다음 프레임 렌더의 or_default()가 빈 항목을
        // 재생성한다 — 계약은 "내용이 비워짐"이다.
        assert!(
            harness.state().0.inputs.values().all(|buf| buf.is_empty()),
            "전송 후 입력 버퍼 내용이 비워져야 한다"
        );
    }

    #[test]
    fn kittest_이동_클릭이_goto_타깃을_만든다() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let cards = vec![card(7)];
        let mut harness = waiting_harness(&catalog, &cards);
        harness.get_by_label("Go to →").click();
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![WaitingAction::Goto(
                crate::ui::notifications::AgentNotificationTarget::Pty {
                    workspace_id: "ws-1".to_owned(),
                    session: SessionId(7),
                }
            )]
        );
    }
}
