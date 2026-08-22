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
/// 본문(Source) 편집기의 왼쪽 여백. 헤더 탭 제목·툴바 라벨과 **같은 세로선**에
/// 맞춘다(2026-08-22 사용자 요청) — 셋이 제각각이면 pane 왼쪽이 계단처럼 보인다.
const SOURCE_EDITOR_LEFT_MARGIN: i8 = crate::ui::workspace::PANE_HEADER_TITLE_LEFT as i8;
/// 원문 줄 간격 배수 — 고정폭 행 높이의 기본(약 1.2배)은 마크다운 원문을 읽기엔
/// 촘촘하다(2026-08-22 사용자 지적). 미리보기와 달리 원문은 우리가 직접 그리므로
/// 여기서는 바꿀 수 있다.
const SOURCE_EDITOR_LINE_HEIGHT: f32 = 1.75;

/// 세그먼트 칸의 좌우 안쪽 여백.
const SEGMENT_PADDING_X: f32 = 8.0;
/// 툴바 왼쪽 여백 — **역산**한 값이다. 세그먼트 첫 라벨("본문")의 첫 글자가 헤더 탭
/// 제목("nomorevibe")과 **같은 x**에 오도록 `여백 + 칸 안쪽 여백`이
/// `PANE_HEADER_TITLE_LEFT`와 같아지게 맞춘다(2026-08-22 사용자 요청). 둘 중 하나만
/// 바꾸면 어긋나므로 상수에서 뽑아 계산한다.
const TOOLBAR_LEFT_MARGIN: i8 =
    (crate::ui::workspace::PANE_HEADER_TITLE_LEFT - SEGMENT_PADDING_X) as i8;

pub fn toolbar(
    ui: &mut egui::Ui,
    snapshot: &DocumentToolbarSnapshot,
    text: &i18n::Catalog,
) -> Option<DocumentToolbarAction> {
    let tokens = crate::ui::designall::tokens(ui.visuals());
    let mut action = None;
    // 툴바 바탕은 **헤더 탭 줄과 같은 면**이다(2026-08-23 사용자 요청) — 탭과 툴바가
    // 한 덩어리로 읽히고, 그 아래 본문만 다른 단으로 갈린다. 헤더가 쓰는 것과 같은
    // 상수를 그대로 가져와 둘이 따로 놀 수 없게 한다(`pane_header_style` 참고 —
    // 테마와 무관하게 항상 다크인 면이라 tokens가 아니라 렌더러 상수를 쓴다).
    //
    // 툴바 아래 구분선은 긋지 않는다 — 면이 갈리므로 선이 없어도 경계가 보인다.
    egui::Frame::NONE
        .fill(terminal::renderer_egui::TERMINAL_SURFACE_BG)
        .inner_margin(egui::Margin {
            left: TOOLBAR_LEFT_MARGIN,
            right: 12,
            top: 2,
            bottom: 2,
        })
        .show(ui, |ui| {
            // **`with_layout`을 쓰지 않는다.** 그건 부모의 남은 높이를 통째로 물려받아,
            // `Align::Center`가 툴바 한 줄이 아니라 pane 본문 전체 높이를 기준으로
            // 세로 가운데 정렬을 해버린다 — 툴바가 화면 한복판까지 내려간다
            // (2026-08-22 실측). `horizontal`은 높이를 한 줄로 묶고 그 안에서 이미
            // 세로 가운데(Align::Center)로 배치한다.
            ui.horizontal(|ui| {
                if snapshot.show_mode_toggle {
                    // 세그먼트 컨트롤 — 테두리 1px(tokens.separator) 안에 활성/비활성 두
                    // 상태만 있다. 헤더 보조 탭(`workspace.rs`의 `render_aux_tab`)이 쓰는
                    // 것과 같은 규칙(활성 tokens.text · 비활성 tokens.muted_text)이라 두
                    // 화면의 "선택됨" 표시가 같은 언어로 읽힌다.
                    // 테두리를 두지 않는다(2026-08-22 사용자 요청) — 좁은 pane에서
                    // 선이 겹치면 답답해진다. 활성 칸만 면으로 올리고 나머지는 글자색만
                    // 낮춘다: 헤더 탭이 활성일 때와 같은 규칙이다.
                    {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            // 세 칸의 **안쪽 여백을 똑같이** 고정한다. 예전엔
                            // 비활성 칸만 `frame(false)`라 프레임 패딩이 빠져
                            // 칸마다 폭·높이가 달라졌다 — 그게 얼라인이 어긋나
                            // 보이던 원인이다(2026-08-22 사용자 지적).
                            // 툴바 줄 높이는 이 칸의 높이가 정한다(텍스트 + 위아래
                            // 여백). 높이를 조절할 땐 **프레임 여백이 아니라 이 값**을
                            // 쓴다 — 여백은 정수라 홀수 px을 나누면 위아래가 비대칭이
                            // 되고, 그러면 글자가 한쪽으로 밀려 보인다(2026-08-22).
                            // 2.5 → 5.475로 줄 전체를 5.95px 높였다(사용자 요청, 여러
                            // 번에 나눠 조정). 마지막 0.5px은 툴바 하단선을 사이드바
                            // 행 경계에 맞추려고 내린 것이다 — 프레임 여백은 정수라
                            // 소수 조정이 안 되고, 이 값만 소수를 받는다.
                            ui.spacing_mut().button_padding = egui::vec2(SEGMENT_PADDING_X, 5.475);
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
                                // 선택 표시는 **글자색만**으로 한다(2026-08-22 사용자
                                // 요청) — 배경을 칠하면 좁은 툴바에서 그 덩어리가
                                // 도드라진다. 활성 `tokens.text`(밝음) · 비활성
                                // `tokens.muted_text`는 위 `label`이 이미 정한다.
                                //
                                // 세 칸 모두 프레임을 켠 채 투명하게 둔다 — 끄면
                                // 패딩까지 사라져 칸 크기가 서로 달라진다.
                                let button = egui::Button::new(label)
                                    .frame(true)
                                    .selected(active)
                                    .stroke(egui::Stroke::NONE)
                                    .fill(egui::Color32::TRANSPARENT);
                                if ui.add(button).clicked() && !active {
                                    action = Some(DocumentToolbarAction::SetMode(mode));
                                }
                            }
                        });
                    }
                    ui.add_space(12.0);
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
            // 원문은 **고정폭**이어야 한다 — `TextEdit`의 기본은 비례 폰트라 표·
            // 들여쓰기·코드 펜스가 어긋나 보였다(2026-08-22 사용자 스크린샷).
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            let row = ui.fonts_mut(|fonts| fonts.row_height(&font));
            ui.add(
                egui::TextEdit::multiline(source)
                    .id_salt(id_salt)
                    .interactive(editable)
                    .font(font.clone())
                    // 배경·테두리 없는 프레임에 **여백만** 싣는다.
                    //
                    // `.margin(..)`은 여기서 쓸 수 없다 — egui는 프레임을 명시하면
                    // margin을 버린다(`builder.rs`: `frame.unwrap_or_else(|| ...
                    // .inner_margin(margin))`). 그래서 margin만 주면 여백이 통째로
                    // 무시돼 첫 글자가 경계선에 붙는다(2026-08-22 실측).
                    //
                    // 컨테이너를 하나 더 끼우는 방법은 쓰지 않는다 — 위젯 계층이
                    // 바뀌어 `app.rs`의 `document_source_editor_state_id` 공식이
                    // 어긋난다(문서를 닫을 때 undo 기록을 지우는 그 id다).
                    .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                        left: SOURCE_EDITOR_LEFT_MARGIN,
                        right: 0,
                        top: 2,
                        bottom: 0,
                    }))
                    .desired_width(f32::INFINITY)
                    .layouter(&mut |ui, text, wrap_width| {
                        let mut job = egui::text::LayoutJob::simple(
                            text.as_str().to_owned(),
                            font.clone(),
                            ui.visuals().text_color(),
                            wrap_width,
                        );
                        // 줄 간격은 섹션 단위로만 줄 수 있다 — `TextEdit`은 우리가
                        // job을 만들어 넘기므로 여기서 지정한다.
                        for section in &mut job.sections {
                            section.format.line_height = Some(row * SOURCE_EDITOR_LINE_HEIGHT);
                        }
                        ui.fonts_mut(|fonts| fonts.layout_job(job))
                    }),
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
