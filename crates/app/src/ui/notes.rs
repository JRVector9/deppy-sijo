//! 사이드바 「메모」 탭 — 워크스페이스당 스크래치패드 한 장.
//!
//! UI leaf라 DB를 만지지 않는다. 저장된 본문은 스냅샷으로 받고, 편집은
//! [`NotesAction::Edited`]로 올려보내 App이 worker 경계에서 기록한다.
//!
//! 이 leaf가 지는 책임은 **버퍼 소유권** 하나다. 편집 중인 문자열은 여기 있고,
//! 스냅샷은 워크스페이스가 바뀔 때만 버퍼를 교체한다 — 매 프레임 덮어쓰면 저장이
//! 한 박자 늦는 사이에 방금 친 글자가 되돌아간다(자동 저장이 디바운스라 반드시 생긴다).

use crate::ui::designall;

/// 이 프레임에 leaf가 받는 입력.
pub struct NotesInput<'a> {
    /// 지금 보고 있는 워크스페이스. 이 값이 바뀌면 버퍼를 교체한다.
    pub workspace_id: &'a str,
    /// DB에 저장된 본문. 미작성이면 `None`.
    pub stored: Option<&'a str>,
}

/// leaf가 App으로 올려보내는 것.
pub enum NotesAction {
    /// 본문이 바뀌었다. App이 디바운스해 DB에 쓴다.
    Edited(String),
}

#[derive(Default)]
pub struct NotesUi {
    buffer: String,
    /// 버퍼가 어느 워크스페이스 것인지. `None`이면 아직 아무것도 안 실었다.
    loaded_workspace: Option<String>,
    /// 다음 렌더에서 텍스트 영역에 커서를 놓는다(탭 진입 시 App/사이드바가 요청).
    focus_pending: bool,
}

impl NotesUi {
    pub fn new() -> Self {
        Self::default()
    }

    /// 「메모」 탭에 들어올 때 호출한다 — 다음 렌더에서 커서가 바로 잡힌다.
    pub fn request_focus(&mut self) {
        self.focus_pending = true;
    }

    fn sync(&mut self, input: &NotesInput<'_>) {
        if self.loaded_workspace.as_deref() == Some(input.workspace_id) {
            // 같은 워크스페이스면 **절대 덮어쓰지 않는다**. 자동 저장이 디바운스라
            // stored는 항상 버퍼보다 뒤쳐져 있고, 여기서 되돌리면 타이핑이 씹힌다.
            return;
        }
        self.buffer = input.stored.unwrap_or_default().to_owned();
        self.loaded_workspace = Some(input.workspace_id.to_owned());
    }

    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
        input: NotesInput<'_>,
        catalog: &i18n::Catalog,
    ) -> Option<NotesAction> {
        self.sync(&input);

        // ⌘⇧D — 커서 위치에 오늘 날짜 줄을 삽입한다. 반드시 메모 TextEdit이
        // 포커스를 쥐고 있을 때만 반응해야 한다(다른 곳에서 눌렀는데 메모가
        // 바뀌면 안 된다는 요구사항). 전역 단축키 디스패처
        // (`App::handle_configured_shortcut`)는 정반대 조건(`!ctx.text_edit_focused()`)
        // 에서만 동작해 텍스트 편집 중엔 통째로 꺼져 있으므로 그 경로를 거치지
        // 않고 여기서 직접 소비한다. 위젯이 이번 프레임에 그려지기 **전에**
        // 버퍼/커서를 바꿔야 같은 프레임에 반영된다(composer.rs의
        // consume-before-draw 관례와 동일).
        let mut date_inserted = false;
        if ui.ctx().memory(|memory| memory.has_focus(text_id()))
            && consume_key_exact(ui.ctx(), NOTE_DATE_MODIFIERS, NOTE_DATE_KEY)
        {
            let char_cursor: usize = egui::text_edit::TextEditState::load(ui.ctx(), text_id())
                .and_then(|state| state.cursor.char_range())
                .map(|range| range.primary.index.into())
                .unwrap_or_else(|| self.buffer.chars().count());
            let byte_cursor = self
                .buffer
                .char_indices()
                .nth(char_cursor)
                .map(|(byte, _)| byte)
                .unwrap_or(self.buffer.len());
            let (new_buffer, new_byte_cursor) =
                insert_date_line(&self.buffer, byte_cursor, &today_local_date());
            if new_buffer != self.buffer {
                self.buffer = new_buffer;
                let new_char_cursor = self.buffer[..new_byte_cursor].chars().count();
                let mut state =
                    egui::text_edit::TextEditState::load(ui.ctx(), text_id()).unwrap_or_default();
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(new_char_cursor),
                    )));
                state.store(ui.ctx(), text_id());
                date_inserted = true;
            }
        }

        let tokens = designall::tokens(ui.visuals());

        // 메모칸은 **자기 테두리를 그리지 않는다**. 상·좌·우는 이미 탭 구분선과 사이드바
        // 경계가 담당하고 있어서, 상자를 치면 같은 자리에 선이 두 겹으로 겹친다
        // (2026-08-10 사용자 지적: 가로세로 두 줄씩). 여백은 1px만 둔다.
        const INSET: f32 = 1.0;

        // 테두리는 `.frame()`으로 없앤다. `visuals.selection.stroke`를 0으로 만들면 안 된다 —
        // egui가 그 값을 **포커스 테두리와 「선택된 글자 색」 양쪽에** 쓰기 때문에
        // (`text_edit/builder.rs:704`, `text_selection/visuals.rs:40`) 드래그한 글자가
        // 투명해져 사라진다(2026-08-10 실증). `.frame()`을 주면 egui가 기본 테두리 로직을
        // 통째로 건너뛰므로 선택 색은 그대로 남는다.
        let borderless = egui::Frame::NONE
            .fill(tokens.workspace_background)
            .inner_margin(egui::Margin::symmetric(8, 6));

        // 뷰포트(보이는 칸)의 자리를 먼저 잡는다. 밑줄은 스크롤과 함께 흘러가면 안 되므로
        // 이 사각형 기준으로 그린다.
        let viewport = egui::Rect::from_min_size(
            ui.cursor().min + egui::vec2(INSET, INSET),
            egui::vec2(
                ui.available_width() - INSET * 2.0,
                ui.available_height() - INSET * 2.0,
            ),
        );
        // 짧은 메모여도 칸 전체가 클릭 대상이어야 한다 — 빈 곳을 눌렀는데 포커스가
        // 안 잡히면 "왜 안 되지"가 된다. 보이는 높이만큼을 최소 줄 수로 요구한다.
        let row_height = ui.text_style_height(&egui::TextStyle::Body);
        let min_rows = ((viewport.height() - 12.0) / row_height).floor().max(3.0) as usize;

        let response = egui::Frame::NONE
            .inner_margin(egui::Margin::same(INSET as i8))
            .show(ui, |ui| {
                // 내용이 칸을 넘으면 **안에서** 스크롤한다. 사이드바 전체가 밀리거나
                // 글이 잘려 나가면 안 된다(2026-08-10 사용자 지적).
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.buffer)
                                .id(text_id())
                                .hint_text(catalog.t("notes.placeholder", &[]))
                                .frame(borderless)
                                // 내용에 따라 세로로 자란다 — 스크롤은 바깥 ScrollArea가 맡는다.
                                .desired_rows(min_rows)
                                .desired_width(f32::INFINITY),
                        )
                    })
                    .inner
            })
            .inner;

        // 하단 한 줄만 둔다 — 포커스 색은 **쓰지 않는다**(2026-08-10 사용자: 정확하게
        // 안 보이게). 탭의 선택 인디케이터가 이미 「메모」에 있어 어디 있는지 알 수 있고,
        // 커서 자체가 깜빡이므로 테두리로 한 번 더 말할 이유가 없다.
        ui.painter().hline(
            viewport.x_range(),
            crate::ui::snap_line_to_pixel(
                viewport.bottom(),
                designall::SEPARATOR_WIDTH,
                ui.ctx().pixels_per_point(),
            ),
            egui::Stroke::new(designall::SEPARATOR_WIDTH, tokens.separator),
        );

        if std::mem::take(&mut self.focus_pending) {
            response.request_focus();
        }

        (date_inserted || response.changed()).then(|| NotesAction::Edited(self.buffer.clone()))
    }

    /// 터미널 선택 등 **밖에서** 들어오는 편집을 버퍼에 즉시 반영한다(PR-4: 「메모에
    /// 추가」). `sync()`와 달리 같은 워크스페이스여도 **덮어쓴다** — stale snapshot이
    /// 아니라 방금 확정된 새 편집이기 때문이다. loaded_workspace를 함께 맞춰 두면
    /// 다음 sync()가 이 값을 stored로 되돌리지 않는다.
    pub fn apply_external_edit(&mut self, workspace_id: &str, body: String) {
        self.buffer = body;
        self.loaded_workspace = Some(workspace_id.to_owned());
    }
}

/// 메모 TextEdit 위젯 id — 포커스/커서 판정과 위젯 자신이 같은 값을 써야 한다.
fn text_id() -> egui::Id {
    egui::Id::new("sidebar_notes_edit")
}

/// 날짜 삽입 단축키 — ⌘⇧D. `shortcuts.rs`의 두 플랫폼 기본값 표(unix/windows) 어디에도
/// 이 조합이 쓰이지 않음을 확인했다(SplitHorizontal이 애초에 Windows Ctrl+Shift+D
/// 충돌 때문에 ⌘⇧D를 비우고 H로 옮긴 자리다). 전역 레지스트리에 넣지 않는 이유는
/// 위 render()의 주석 참고 — 텍스트 편집 포커스 중엔 전역 디스패처가 꺼져 있어
/// 애초에 그쪽과 실행 충돌이 날 수 없다.
const NOTE_DATE_MODIFIERS: egui::Modifiers = egui::Modifiers {
    alt: false,
    ctrl: false,
    shift: true,
    mac_cmd: false,
    command: true,
};
const NOTE_DATE_KEY: egui::Key = egui::Key::D;

/// 정확히 이 (modifiers, key) 조합의 key-down만 소비한다 — repeat는 걸러 키를 누르고
/// 있어도 한 번만 삽입되게 한다. composer.rs의 `consume_key_exact`(Enter 전송용)와
/// 계약은 같지만 그쪽은 repeat를 걸러내지 않는다 — 이 액션은 연타/홀드가 아니라
/// 단발 삽입이라 repeat까지 잡으면 키를 누르고 있는 동안 날짜 줄이 계속 늘어난다.
fn consume_key_exact(ctx: &egui::Context, modifiers: egui::Modifiers, key: egui::Key) -> bool {
    ctx.input_mut(|input| {
        let mut consumed = false;
        input.events.retain(|event| {
            if consumed {
                return true;
            }
            let egui::Event::Key {
                key: event_key,
                pressed: true,
                repeat: false,
                modifiers: event_modifiers,
                ..
            } = event
            else {
                return true;
            };
            if *event_key == key && event_modifiers.matches_exact(modifiers) {
                consumed = true;
                return false;
            }
            true
        });
        consumed
    })
}

/// 오늘 날짜를 로컬 시간대 "YYYY-MM-DD"로. std에는 시간대 정보가 없어 unix에서는
/// 앱 전역에서 이미 쓰는 libc의 `localtime_r`로 OS가 계산한 오프셋을 읽는다
/// (status_feed.rs의 관례와 동일 — 이 워크스페이스는 chrono를 쓰지 않는다).
#[cfg(unix)]
fn today_local_date() -> String {
    let now = deppy_core::time::unix_secs_i64();
    deppy_core::time::civil_date(now + local_utc_offset_secs(now))
}

#[cfg(not(unix))]
fn today_local_date() -> String {
    deppy_core::time::civil_date(deppy_core::time::unix_secs_i64())
}

/// 로컬 시간대 오프셋(초) — status_feed.rs의 `local_utc_offset_secs`와 같은 계산을
/// 여기서도 쓴다(파일 간 결합을 늘리지 않으려 작은 함수를 그대로 복제했다).
#[cfg(unix)]
fn local_utc_offset_secs(utc_secs: i64) -> i64 {
    let time = utc_secs as libc::time_t;
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: parts는 스택에 있고 localtime_r이 채운다. 실패하면 널을 돌려주므로
    // 그때는 UTC(0)로 떨어진다.
    if unsafe { libc::localtime_r(&time, &mut parts) }.is_null() {
        return 0;
    }
    parts.tm_gmtoff as i64
}

/// 커서 위치에 날짜 줄을 삽입한다. 순수 함수 — UI/IO 의존이 없어 시각과 무관하게
/// 단위 테스트할 수 있다. `cursor`는 `buffer`의 **문자 경계 바이트 오프셋**이어야
/// 한다(호출측이 egui의 문자 인덱스 커서를 변환해 넘긴다). 반환값은
/// (새 버퍼, 새 커서의 바이트 오프셋).
///
/// - 커서가 줄 중간이면 그 줄을 쪼개 날짜를 자기 줄로 끼워 넣는다.
/// - 커서 다음에 아무것도 없으면(버퍼 끝/빈 버퍼) 날짜 뒤에 빈 줄을 하나 남겨
///   바로 이어서 메모를 적을 수 있게 한다.
/// - 커서 바로 위 줄이 이미 `date`와 같으면 삽입하지 않고 원본을 그대로 돌려준다
///   (단축키를 거듭 눌러도 날짜 줄이 중복되지 않게).
fn insert_date_line(buffer: &str, cursor: usize, date: &str) -> (String, usize) {
    let before = &buffer[..cursor];
    let after = &buffer[cursor..];

    let already_dated = before == date || before.ends_with(&format!("{date}\n"));
    if already_dated {
        return (buffer.to_owned(), cursor);
    }

    let pre = if before.is_empty() || before.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let post = if after.starts_with('\n') { "" } else { "\n" };

    let mut result = String::with_capacity(buffer.len() + pre.len() + date.len() + post.len());
    result.push_str(before);
    result.push_str(pre);
    result.push_str(date);
    result.push_str(post);
    result.push_str(after);

    let new_cursor = before.len() + pre.len() + date.len() + post.len();
    (result, new_cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> i18n::Catalog {
        i18n::Catalog::load("ko-KR").unwrap()
    }

    /// 워크스페이스가 바뀌면 버퍼가 새 워크스페이스 본문으로 **교체**돼야 한다.
    /// 안 바꾸면 A에서 쓰던 글이 B 화면에 뜨고, 그대로 저장되면 B의 메모가 오염된다.
    #[test]
    fn 워크스페이스가_바뀌면_버퍼를_교체한다() {
        let mut notes = NotesUi::new();
        notes.sync(&NotesInput {
            workspace_id: "ws-a",
            stored: Some("A의 메모"),
        });
        assert_eq!(notes.buffer, "A의 메모");

        notes.sync(&NotesInput {
            workspace_id: "ws-b",
            stored: Some("B의 메모"),
        });
        assert_eq!(notes.buffer, "B의 메모");

        // 메모가 없는 워크스페이스로 가면 빈 버퍼 — 이전 글이 남으면 안 된다.
        notes.sync(&NotesInput {
            workspace_id: "ws-c",
            stored: None,
        });
        assert_eq!(notes.buffer, "");
    }

    /// 같은 워크스페이스에서 재렌더될 때 stored로 덮어쓰면 **타이핑이 씹힌다**.
    /// 자동 저장은 디바운스라 stored가 버퍼보다 항상 뒤쳐져 있다 — 이 규칙이 없으면
    /// 빠르게 치는 동안 몇 글자가 주기적으로 사라진다.
    #[test]
    fn 같은_워크스페이스에서는_stored가_버퍼를_덮어쓰지_않는다() {
        let mut notes = NotesUi::new();
        notes.sync(&NotesInput {
            workspace_id: "ws-a",
            stored: Some("저장된 값"),
        });
        notes.buffer.push_str(" + 방금 친 글자");

        // 아직 DB에는 옛 값만 있다(디바운스 대기 중).
        notes.sync(&NotesInput {
            workspace_id: "ws-a",
            stored: Some("저장된 값"),
        });
        assert_eq!(notes.buffer, "저장된 값 + 방금 친 글자");
    }

    /// 탭에 들어오면 커서가 잡혀야 하고, 요청은 **한 번만** 소비돼야 한다.
    /// 매 프레임 재요청하면 사용자가 다른 곳을 클릭해도 포커스가 도로 끌려온다.
    #[test]
    fn 포커스_요청은_한번만_소비된다() {
        let mut notes = NotesUi::new();
        assert!(!notes.focus_pending);
        notes.request_focus();
        assert!(notes.focus_pending);
        assert!(std::mem::take(&mut notes.focus_pending));
        assert!(!notes.focus_pending);
    }

    /// 편집하면 Edited가 나오고, 안 건드리면 아무것도 안 나온다.
    #[test]
    fn kittest_편집하면_edited가_올라간다() {
        let catalog = catalog();
        let catalog_ref = &catalog;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(220.0, 300.0))
            .build_ui_state(
                move |ui, state: &mut (NotesUi, Option<NotesAction>)| {
                    let (notes, out) = state;
                    let action = notes.render(
                        ui,
                        NotesInput {
                            workspace_id: "ws-a",
                            stored: Some("처음"),
                        },
                        catalog_ref,
                    );
                    if action.is_some() {
                        *out = action;
                    }
                },
                (NotesUi::new(), None),
            );
        harness.run();
        assert!(
            harness.state().1.is_none(),
            "건드리지 않았는데 저장 액션이 올라갔다"
        );

        harness.state_mut().0.buffer.push_str(" 추가");
        harness.run();
        // 버퍼를 밖에서 바꾼 것은 TextEdit의 changed()가 아니므로 액션이 없다.
        // 대신 sync가 덮어쓰지 않았음을 확인한다(위 단위 테스트의 렌더 경로 확인).
        assert_eq!(harness.state().0.buffer, "처음 추가");
    }

    // ── ⌘⇧D 날짜 삽입 — 순수 삽입 로직(`insert_date_line`) 경계 테스트 ──
    //
    // 시각에 의존하지 않도록 날짜 문자열을 인자로 받는다(호출측이 실제 오늘 날짜를
    // 채운다). `cursor`는 `buffer`의 문자 경계 바이트 오프셋이다.

    const DATE: &str = "2026-08-10";

    /// 빈 버퍼면 날짜 한 줄만 남고, 그 아래 빈 줄에 커서가 놓인다.
    #[test]
    fn insert_date_line_빈_버퍼() {
        assert_eq!(
            insert_date_line("", 0, DATE),
            ("2026-08-10\n".to_owned(), 11)
        );
    }

    /// 줄 중간이면 그 줄을 쪼개 날짜 줄을 끼워 넣는다 — 뒷부분은 다음 줄로 밀린다.
    #[test]
    fn insert_date_line_줄_중간() {
        assert_eq!(
            insert_date_line("abcdef", 3, DATE),
            ("abc\n2026-08-10\ndef".to_owned(), 15)
        );
    }

    /// 줄 끝(=버퍼 끝)이면 새 줄로 날짜가 붙고 커서는 그 아래 빈 줄로 간다.
    #[test]
    fn insert_date_line_줄_끝() {
        assert_eq!(
            insert_date_line("todo item", 9, DATE),
            ("todo item\n2026-08-10\n".to_owned(), 21)
        );
    }

    /// 맨 앞이면 기존 글 위에 날짜 줄이 붙고, 기존 글은 그대로 다음 줄에 남는다.
    #[test]
    fn insert_date_line_맨_앞() {
        assert_eq!(
            insert_date_line("existing text", 0, DATE),
            ("2026-08-10\nexisting text".to_owned(), 11)
        );
    }

    /// 커서 바로 위 줄이 이미 같은 날짜면 중복 삽입하지 않는다 — 단축키를 두 번
    /// 눌러도 날짜 줄이 겹겹이 쌓이지 않게.
    #[test]
    fn insert_date_line_바로_위에_같은_날짜가_있으면_중복_삽입하지_않는다() {
        assert_eq!(
            insert_date_line("2026-08-10\n", 11, DATE),
            ("2026-08-10\n".to_owned(), 11)
        );
        // 개행이 아직 없는 경우(막 삽입된 직후 등)도 동일하게 막는다.
        assert_eq!(
            insert_date_line("2026-08-10", 10, DATE),
            ("2026-08-10".to_owned(), 10)
        );
    }

    /// 멀티바이트(한글) 문자 경계에서도 패닉 없이 바이트 오프셋으로 정확히 쪼갠다.
    #[test]
    fn insert_date_line_한글_커서_경계() {
        // "안"(3바이트) + "녕"(3바이트) 뒤, 바이트 오프셋 6.
        assert_eq!(
            insert_date_line("안녕하세요", 6, DATE),
            (
                "안녕\n2026-08-10\n하세요".to_owned(),
                6 + 1 + DATE.len() + 1
            )
        );
    }

    /// ⌘⇧D가 이 플랫폼의 전역 단축키 기본값과 겹치지 않는지 고정한다. 겹쳐도
    /// 전역 디스패처(`handle_configured_shortcut`)는 텍스트 편집 중엔 통째로
    /// 꺼져 있어(`ctx.text_edit_focused()`면 조기 반환) 실행 충돌은 나지 않지만,
    /// 같은 조합을 두 의미로 쓰면 사용자가 헷갈린다. 2026-08-09
    /// PromptJumpPrev/Ctrl+Shift+Up 충돌 사고 이후 새 기본값에도 이 검증을 남긴다.
    #[test]
    fn 날짜_삽입_단축키는_전역_기본_바인딩과_겹치지_않는다() {
        let config = crate::config::ShortcutsConfig::default();
        for action in crate::shortcuts::ShortcutAction::ALL {
            if let Some(binding) = crate::shortcuts::effective_binding(&config, action) {
                assert_ne!(
                    crate::shortcuts::serialize_binding(binding),
                    "Command+Shift+D",
                    "{}의 기본 바인딩이 날짜 삽입과 겹친다",
                    action.id()
                );
            }
        }
    }

    /// 메모 TextEdit이 포커스를 쥐고 있을 때 ⌘⇧D를 누르면 커서 위치에 날짜가
    /// 꽂히고 `Edited`가 올라간다(디바운스 저장이 걸리려면 반드시 나가야 한다).
    /// 포커스가 없으면 아무 일도 없어야 한다 — 다른 곳에서 눌렀는데 메모가
    /// 바뀌면 안 된다는 요구사항의 회귀 방지.
    #[test]
    fn kittest_포커스_중_날짜_단축키를_누르면_삽입되고_edited가_올라간다() {
        let catalog = catalog();
        let catalog_ref = &catalog;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(220.0, 300.0))
            .build_ui_state(
                move |ui, state: &mut (NotesUi, Option<NotesAction>)| {
                    let (notes, out) = state;
                    let action = notes.render(
                        ui,
                        NotesInput {
                            workspace_id: "ws-a",
                            stored: Some("메모"),
                        },
                        catalog_ref,
                    );
                    if action.is_some() {
                        *out = action;
                    }
                },
                (NotesUi::new(), None),
            );
        harness.run();
        assert!(harness.state().1.is_none(), "포커스 전인데 액션이 올라갔다");

        harness.state_mut().0.request_focus();
        harness.run();

        harness.key_press_modifiers(
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            egui::Key::D,
        );
        harness.run();

        let buffer = harness.state().0.buffer.clone();
        assert!(
            buffer.starts_with("메모\n") && buffer.trim_end() != "메모",
            "날짜 줄이 삽입되지 않았다: {buffer:?}"
        );
        assert!(
            matches!(&harness.state().1, Some(NotesAction::Edited(body)) if body == &buffer),
            "삽입 후 Edited가 올라가야 저장 디바운스가 걸린다"
        );
    }

    /// 밖에서 들어온 편집(터미널 선택 → 메모에 추가)은 `sync()`와 달리 같은
    /// 워크스페이스여도 버퍼를 덮어써야 한다 — 그래야 「메모」 탭을 이미 열어 둔
    /// 채로 터미널에서 추가해도 화면에 바로 보인다.
    #[test]
    fn 밖에서_들어온_편집은_같은_워크스페이스여도_버퍼를_덮는다() {
        let mut notes = NotesUi::new();
        notes.sync(&NotesInput {
            workspace_id: "ws-a",
            stored: Some("원래 메모"),
        });

        notes.apply_external_edit("ws-a", "원래 메모\n추가된 줄".to_owned());
        assert_eq!(notes.buffer, "원래 메모\n추가된 줄");

        // 이후 sync가 stored(옛 값)로 되돌리면 안 된다 — loaded_workspace가 이미 ws-a.
        notes.sync(&NotesInput {
            workspace_id: "ws-a",
            stored: Some("원래 메모"),
        });
        assert_eq!(notes.buffer, "원래 메모\n추가된 줄");
    }
}
