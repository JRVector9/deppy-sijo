//! 문서 탭 leaf — 툴바 · source 편집기(설계 §5). intent만 돌려준다 — 파일 IO는 절대
//! 하지 않는다.
//!
//! 설계: `docs/superpowers/specs/2026-08-21-document-tab-design.md` §3·§4·§5·§6.
//!
//! Split(나란히) 레이아웃의 좌우 분할·드래그는 이력·Git 보조 탭과 같은 규칙
//! (`aux_split_width`/`git_tab_split_width` 관례)이라 App(`app.rs`)이 직접 담당한다 —
//! 이 파일은 그 안에 올라가는 두 조각(툴바 한 줄, source 편집기 한 칸)만 그린다.

/// source / preview / split 토글값(설계 §4 `DocumentViewMode`). 평문(`.txt` 등)은
/// Source 고정 — 토글 자체를 숨긴다(§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentViewMode {
    Source,
    Preview,
    Split,
}

/// 툴바 한 프레임 입력.
pub struct DocumentToolbarSnapshot {
    pub mode: DocumentViewMode,
    /// 평문 등 Markdown이 아닌 파일이면 거짓 — 모드 토글 자체를 숨긴다(§3.1).
    pub show_mode_toggle: bool,
    /// 저장 버튼 활성 판정 — dirty && Full 티어 && 저장 요청이 이미 나가 있지 않음.
    pub can_save: bool,
    /// 상태 문구 — 이미 로케일로 옮겨진 문자열이다(App이 이유를 판정해 문구까지
    /// 고른다, leaf는 그리기만 한다). `None`이면 깨끗해 아무것도 보이지 않는다.
    pub status_text: Option<String>,
}

/// 툴바가 올리는 intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentToolbarAction {
    SetMode(DocumentViewMode),
    Save,
}

/// 문서 탭 툴바 한 줄 — 모드 토글 / 저장 버튼 / 상태 문구(§3). `⌘S`도 여기서 함께
/// 소비한다 — 이 함수는 문서 탭이 활성인 프레임에서만 호출되므로(app.rs), 다른
/// 화면에서 `⌘S`가 걸릴 일이 없다.
pub fn toolbar(
    ui: &mut egui::Ui,
    snapshot: &DocumentToolbarSnapshot,
    text: &i18n::Catalog,
) -> Option<DocumentToolbarAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        if snapshot.show_mode_toggle {
            for (mode, key) in [
                (DocumentViewMode::Source, "document.mode.source"),
                (DocumentViewMode::Preview, "document.mode.preview"),
                (DocumentViewMode::Split, "document.mode.split"),
            ] {
                if ui
                    .selectable_label(snapshot.mode == mode, text.t(key, &[]))
                    .clicked()
                    && snapshot.mode != mode
                {
                    action = Some(DocumentToolbarAction::SetMode(mode));
                }
            }
            ui.separator();
        }

        let save_clicked = ui
            .add_enabled(
                snapshot.can_save,
                egui::Button::new(text.t("document.save", &[])),
            )
            .clicked();
        // 문서 탭이 활성인 동안에만 호출되는 함수라 여기서 그대로 소비해도 다른 화면의
        // `⌘S`를 가로채지 않는다(notes.rs `⌘⇧D`와 같은 근거 — 전역 단축키 디스패처는
        // TextEdit 포커스 중엔 이미 꺼져 있고, 포커스가 없어도 이 함수가 먼저 그려진다).
        let save_shortcut = snapshot.can_save
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
        if save_clicked || save_shortcut {
            action = Some(DocumentToolbarAction::Save);
        }

        if let Some(status_text) = &snapshot.status_text {
            ui.weak(status_text);
        }
    });
    action
}

/// source 편집 영역 — egui multiline `TextEdit`. `editable`이 거짓이면
/// `.interactive(false)`로 읽기 전용(ViewOnly 티어, §6). 반환값은 "이번 프레임에
/// 바뀌었는가"뿐이다 — 실제 내용은 `source`에 이미 반영돼 있다(App이 자신의 버퍼를
/// `&mut`로 직접 빌려주므로 leaf가 복사본을 따로 들고 있지 않는다 — 매 키 입력마다
/// 문서 전체를 복제하지 않는다).
///
/// `id_salt`는 호출부(App)가 문서 식별자(경로)에서 만든 안정 id다. 자동 위젯 id에
/// 맡기면 다른 문서로 교체해도 화면상 "같은 자리"라 egui가 같은 위젯으로 보고
/// undo 기록을 이어준다 — 문서 B에서 문서 A의 되돌리기 이력이 튀어나오는 사고로
/// 이어진다. 문서마다 다른 id를 주면 교체 시 자연히 새 위젯이 되어 그 문제가
/// 없다.
pub fn source_editor(
    ui: &mut egui::Ui,
    id_salt: egui::Id,
    source: &mut String,
    editable: bool,
) -> bool {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(source)
                    .id_salt(id_salt)
                    .interactive(editable)
                    .desired_width(f32::INFINITY),
            )
        })
        .inner
        .changed()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        mode: DocumentViewMode,
        can_save: bool,
        status_text: Option<String>,
    ) -> DocumentToolbarSnapshot {
        DocumentToolbarSnapshot {
            mode,
            show_mode_toggle: true,
            can_save,
            status_text,
        }
    }

    struct ToolbarHarnessState {
        snapshot: DocumentToolbarSnapshot,
        catalog: i18n::Catalog,
        last_action: Option<DocumentToolbarAction>,
    }

    #[test]
    fn 툴바_모드_버튼을_누르면_setmode_intent를_올린다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let preview_label = catalog.t("document.mode.preview", &[]);
        let state = ToolbarHarnessState {
            snapshot: snapshot(DocumentViewMode::Source, false, None),
            catalog,
            last_action: None,
        };
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut ToolbarHarnessState| {
                state.last_action = toolbar(ui, &state.snapshot, &state.catalog);
            },
            state,
        );
        harness.run();
        harness.get_by_label(&preview_label).click();
        harness.step();

        assert_eq!(
            harness.state().last_action,
            Some(DocumentToolbarAction::SetMode(DocumentViewMode::Preview))
        );
    }

    #[test]
    fn 저장_버튼이_비활성이면_클릭해도_save_intent가_안_올라온다() {
        use egui_kittest::kittest::Queryable;

        let catalog = i18n::Catalog::load(i18n::FALLBACK_LOCALE).unwrap();
        let save_label = catalog.t("document.save", &[]);
        let state = ToolbarHarnessState {
            snapshot: snapshot(DocumentViewMode::Source, false, None),
            catalog,
            last_action: None,
        };
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, state: &mut ToolbarHarnessState| {
                state.last_action = toolbar(ui, &state.snapshot, &state.catalog);
            },
            state,
        );
        harness.run();
        harness.get_by_label(&save_label).click();
        harness.step();

        assert_eq!(harness.state().last_action, None);
    }

    #[test]
    fn source_editor는_읽기전용이어도_패닉_없이_그려진다() {
        let mut source = "readonly content".to_owned();
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, source: &mut String| {
                source_editor(ui, egui::Id::new("test"), source, false);
            },
            source.clone(),
        );
        harness.run();
        let _ = &mut source; // 위젯이 실제로 그려졌는지는 harness.run()의 패닉 여부로 확인한다.
    }
}
