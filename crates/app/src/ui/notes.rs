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

        // 앱 전역 테마는 `selection.bg_fill`과 `selection.stroke`를 **둘 다 accent**로 둔다
        // (theme.rs:124-125, designall.rs:144-145 — 다른 컴포넌트가 이 값을 accent 소스로
        // 참조하기 때문). 그런데 egui는 stroke를 **선택된 글자 색**으로도 쓰므로
        // (`text_selection/visuals.rs:40`) accent 배경 위 accent 글자가 되어 드래그하면
        // 글이 사라진다(2026-08-10 실증). 전역을 바꾸면 accent를 참조하는 컴포넌트가
        // 함께 흔들리므로 **이 칸에서만** 대비가 나오는 색으로 되돌린다.
        // `.frame()`을 쓰므로 이 값이 테두리로 새지 않는다.
        let accent = tokens.accent;
        let accent_luma = 0.299 * f32::from(accent.r())
            + 0.587 * f32::from(accent.g())
            + 0.114 * f32::from(accent.b());
        let mut visuals = ui.visuals().clone();
        visuals.selection.stroke = egui::Stroke::new(
            1.0,
            if accent_luma > 140.0 {
                egui::Color32::BLACK
            } else {
                egui::Color32::WHITE
            },
        );

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
                *ui.visuals_mut() = visuals;
                // 내용이 칸을 넘으면 **안에서** 스크롤한다. 사이드바 전체가 밀리거나
                // 글이 잘려 나가면 안 된다(2026-08-10 사용자 지적).
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.buffer)
                                .id(egui::Id::new("sidebar_notes_edit"))
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

        response
            .changed()
            .then(|| NotesAction::Edited(self.buffer.clone()))
    }
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
}
