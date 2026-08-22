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
///
/// 색은 전부 `designall::tokens`에서 가져온다 — egui 기본 `selectable_label`/`Button`은
/// `visuals.selection.bg_fill`(시안 accent, `theme.rs`가 전역으로 심는다)로 선택 상태를
/// 칠하는데, 그건 드래그 텍스트 선택과 같은 색이라 "선택된 탭"이 아니라 "드래그된
/// 텍스트"처럼 보인다(2026-08-22 사용자 스크린샷 지적). 그래서 `Button`을 직접 만들어
/// `.fill()`/`.stroke()`로 토큰 색만 명시한다.
pub fn toolbar(
    ui: &mut egui::Ui,
    snapshot: &DocumentToolbarSnapshot,
    text: &i18n::Catalog,
) -> Option<DocumentToolbarAction> {
    let tokens = crate::ui::designall::tokens(ui.visuals());
    let mut action = None;
    // 툴바 바탕 = 본문(Preview 페이지)과 같은 면(content_canvas). 아래 1px 구분선은
    // 호출부(app.rs)가 이 함수 바로 뒤에 `ui.separator()`로 긋는다 — 그 stroke 색은
    // `theme.rs`가 전역으로 `tokens.separator`와 같은 값을 심어 둬 여기서 다시 그릴
    // 필요가 없다.
    // 툴바 바탕은 **본문과 같은 면**이다 — 따로 칠하지 않는다(2026-08-22). 예전엔
    // `content_canvas`를 칠해 본문(pane 면)보다 어두운 띠가 위에 얹혀 보였다.
    // 좌우 여백이 없으면 첫 글자가 pane 모서리에 붙는다.
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 12,
            right: 12,
            top: 5,
            bottom: 5,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if snapshot.show_mode_toggle {
                    // 세그먼트 컨트롤 — 테두리 1px(tokens.separator) 안에 활성/비활성 두
                    // 상태만 있다. 헤더 보조 탭(`workspace.rs`의 `render_aux_tab`)이 쓰는
                    // 것과 같은 규칙(활성 tokens.text · 비활성 tokens.muted_text)이라 두
                    // 화면의 "선택됨" 표시가 같은 언어로 읽힌다.
                    egui::Frame::NONE
                        .stroke(egui::Stroke::new(1.0, tokens.separator))
                        .corner_radius(crate::ui::designall::STRUCTURAL_CORNER_RADIUS)
                        .inner_margin(egui::Margin::same(1))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                // 세 칸의 **안쪽 여백을 똑같이** 고정한다. 예전엔
                                // 비활성 칸만 `frame(false)`라 프레임 패딩이 빠져
                                // 칸마다 폭·높이가 달라졌다 — 그게 얼라인이 어긋나
                                // 보이던 원인이다(2026-08-22 사용자 지적).
                                ui.spacing_mut().button_padding = egui::vec2(9.0, 3.0);
                                for (mode, key) in [
                                    (DocumentViewMode::Source, "document.mode.source"),
                                    (DocumentViewMode::Preview, "document.mode.preview"),
                                    (DocumentViewMode::Split, "document.mode.split"),
                                ] {
                                    let active = snapshot.mode == mode;
                                    let label =
                                        egui::RichText::new(text.t(key, &[])).color(if active {
                                            tokens.text
                                        } else {
                                            tokens.muted_text
                                        });
                                    // 활성/비활성 **둘 다 프레임을 켠다**. 비활성은
                                    // 투명 채움이라 보이지 않지만 패딩이 같아 세 칸의
                                    // 크기가 일치한다.
                                    let button = egui::Button::new(label)
                                        .frame(true)
                                        .selected(active)
                                        .stroke(egui::Stroke::NONE)
                                        .fill(if active {
                                            tokens.selected_background
                                        } else {
                                            egui::Color32::TRANSPARENT
                                        });
                                    if ui.add(button).clicked() && !active {
                                        action = Some(DocumentToolbarAction::SetMode(mode));
                                    }
                                }
                            });
                        });
                    ui.add_space(10.0);
                }

                // 저장 — 텍스트 버튼(프레임 없음). dirty(=can_save)일 때만 accent, 아니면
                // muted. 기본 `egui::Button`은 항상 상자 테두리가 있어 툴바가 "띠"처럼
                // 떠 보였다(2026-08-22 사용자 지적).
                let save_color = if snapshot.can_save {
                    tokens.accent
                } else {
                    tokens.muted_text
                };
                let save_label =
                    egui::RichText::new(text.t("document.save", &[])).color(save_color);
                // 「저장」과 「⌘S」는 한 덩어리로 읽혀야 한다 — 기본 item_spacing이
                // 들어가면 둘이 떨어져 보인다(2026-08-22).
                let shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::S);
                let save_clicked = ui
                    .horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        let clicked = ui
                            .add_enabled(
                                snapshot.can_save,
                                egui::Button::new(save_label).frame(false),
                            )
                            .clicked();
                        ui.add(egui::Label::new(
                            egui::RichText::new(ui.ctx().format_shortcut(&shortcut))
                                .color(tokens.muted_text),
                        ));
                        clicked
                    })
                    .inner;

                // 문서 탭이 활성인 동안에만 호출되는 함수라 여기서 그대로 소비해도 다른
                // 화면의 `⌘S`를 가로채지 않는다(notes.rs `⌘⇧D`와 같은 근거 — 전역 단축키
                // 디스패처는 TextEdit 포커스 중엔 이미 꺼져 있고, 포커스가 없어도 이
                // 함수가 먼저 그려진다).
                let save_shortcut = snapshot.can_save
                    && ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::COMMAND, egui::Key::S)
                    });
                if save_clicked || save_shortcut {
                    action = Some(DocumentToolbarAction::Save);
                }

                if let Some(status_text) = &snapshot.status_text {
                    // 오른쪽 정렬 — 남은 폭을 오른쪽부터 채우는 중첩 레이아웃.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(
                            egui::RichText::new(status_text).color(tokens.muted_text),
                        ));
                    });
                }
            });
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
                    // 기본 프레임(배경 + 테두리)을 끈다 — 켜두면 편집기만 어두운
                    // 상자로 보여 미리보기와 두 물건처럼 갈린다. 고정폭 글꼴이 이미
                    // "원본"임을 말해준다(2026-08-22).
                    .frame(egui::Frame::NONE)
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

    /// 툴바 색은 전부 `designall::tokens`에서 와야 한다 — 리터럴 `Color32`를 직접
    /// 적으면 앱 팔레트 밖의 색(예: 시안 accent 대신 다른 청록)이 슬쩍 섞여 들어올 수
    /// 있다(`activity.rs`의 `production_source_has_no_render_host_or_polling_edges`와
    /// 같은 소스 스캔 관례).
    #[test]
    fn 프로덕션_코드는_색을_리터럴로_적지_않고_토큰에서만_가져온다() {
        let source = include_str!("document.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "Color32::from_rgb",
            "Color32::from_gray",
            "Color32::from_rgba",
            "Color32::WHITE",
            "Color32::BLACK",
        ] {
            assert!(
                !production.contains(forbidden),
                "document.rs leaf는 색을 하드코딩하면 안 된다, 발견: {forbidden}"
            );
        }
    }
}
