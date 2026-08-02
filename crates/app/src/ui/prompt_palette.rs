//! 프롬프트 라이브러리 팔레트 (기능2, leaf).
//!
//! 저장된 프롬프트를 검색·선택해 `{{param}}`를 채워 **컴포저에 삽입**하고(PR-2), 프롬프트를
//! **저장·편집·삭제**한다(PR-3). leaf 경계를 지켜 egui 상태만 소유하고, 실제 삽입·라이브러리
//! 변경은 App이 반환 intent(`Insert`/`Upsert`/`Delete`)를 받아 수행한다 — 삭제·저장은 App이
//! 파일에 영속화한다.

use std::collections::BTreeMap;

use crate::prompt_library::{Prompt, PromptLibrary, parse_tags};

/// 팔레트가 App에 돌려주는 액션. 실제 composer 삽입·라이브러리 변경/영속화는 App이 한다.
pub enum PromptPaletteAction {
    /// 파라미터가 채워진 최종 텍스트를 컴포저 버퍼에 삽입한다.
    Insert(String),
    /// 프롬프트를 추가(신규)하거나 같은 id를 교체(편집)한다.
    Upsert(Prompt),
    /// id로 프롬프트를 삭제한다.
    Delete(String),
}

/// 편집 폼 상태(신규·편집 공용). `id`가 None이면 신규 저장이다.
#[derive(Default)]
struct PromptDraft {
    id: Option<String>,
    title: String,
    body: String,
    /// 공백·쉼표 구분 태그 입력(제출 시 parse_tags로 정규화).
    tags: String,
}

#[derive(Default)]
pub struct PromptPaletteUi {
    open: bool,
    query: String,
    /// 선택된 프롬프트 id(없으면 목록 화면).
    selected: Option<String>,
    /// 선택된 프롬프트의 파라미터 입력값.
    params: BTreeMap<String, String>,
    focus_search: bool,
    /// 편집 폼(Some이면 목록/상세 대신 폼을 그린다).
    editing: Option<PromptDraft>,
    /// 상세 화면 삭제 확인 단계.
    confirm_delete: bool,
}

impl PromptPaletteUi {
    /// 팔레트를 연다(상태 초기화 + 검색창 포커스).
    pub fn open(&mut self) {
        self.open = true;
        self.focus_search = true;
        self.query.clear();
        self.selected = None;
        self.params.clear();
        self.editing = None;
        self.confirm_delete = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 팔레트를 닫는다(설정에서 기능을 끌 때 호출).
    pub fn close(&mut self) {
        self.open = false;
    }

    /// 팔레트를 그린다. 닫혀 있으면 아무것도 안 그리고 None. `composer_draft`는 "현재 컴포저
    /// 내용 저장"을 프리필하기 위한 활성 워크스페이스의 컴포저 입력이다. 삽입(Insert)은
    /// 팔레트를 닫고 컴포저로 돌아가지만, 저장/삭제(Upsert/Delete)는 계속 관리하도록
    /// 팔레트를 열어 둔다.
    pub fn render(
        &mut self,
        ctx: &egui::Context,
        library: &PromptLibrary,
        composer_draft: &str,
        catalog: &i18n::Catalog,
    ) -> Option<PromptPaletteAction> {
        if !self.open {
            return None;
        }
        let mut action = None;
        let mut open = true;
        egui::Window::new(catalog.t("prompt.title", &[]))
            .id(egui::Id::new("prompt_palette"))
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .open(&mut open)
            .show(ctx, |ui| {
                action = self.body(ui, library, composer_draft, catalog);
            });
        // Esc: 편집 중이면 폼만 닫고(목록/상세로 복귀), 아니면 팔레트를 닫는다.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.editing.is_some() {
                self.editing = None;
            } else {
                open = false;
            }
        }
        // 삽입만 팔레트를 닫는다(저장·삭제는 관리 연속성 위해 유지).
        let closing = matches!(action, Some(PromptPaletteAction::Insert(_)));
        self.open = open && !closing;
        action
    }

    fn body(
        &mut self,
        ui: &mut egui::Ui,
        library: &PromptLibrary,
        composer_draft: &str,
        catalog: &i18n::Catalog,
    ) -> Option<PromptPaletteAction> {
        if self.editing.is_some() {
            return self.edit_form(ui, library, catalog);
        }
        match self.selected.clone().and_then(|id| library.get(&id)) {
            Some(prompt) => self.detail_view(ui, prompt, catalog),
            None => {
                self.list_view(ui, library, composer_draft, catalog);
                None
            }
        }
    }

    /// 목록: 상단 액션(새 프롬프트/컴포저 내용 저장) + 검색 + 선택.
    fn list_view(
        &mut self,
        ui: &mut egui::Ui,
        library: &PromptLibrary,
        composer_draft: &str,
        catalog: &i18n::Catalog,
    ) {
        ui.horizontal(|ui| {
            if ui.button(catalog.t("prompt.new", &[])).clicked() {
                self.editing = Some(PromptDraft::default());
            }
            if !composer_draft.trim().is_empty()
                && ui
                    .button(catalog.t("prompt.save_from_composer", &[]))
                    .on_hover_text(catalog.t("prompt.save_from_composer.hint", &[]))
                    .clicked()
            {
                self.editing = Some(PromptDraft {
                    id: None,
                    title: String::new(),
                    body: composer_draft.to_owned(),
                    tags: String::new(),
                });
            }
        });
        ui.separator();
        let search = ui.add(
            egui::TextEdit::singleline(&mut self.query)
                .hint_text(catalog.t("prompt.search", &[]))
                .desired_width(f32::INFINITY),
        );
        if self.focus_search {
            search.request_focus();
            self.focus_search = false;
        }
        ui.add_space(4.0);
        let matches = library.search(&self.query);
        if matches.is_empty() {
            ui.weak(catalog.t("prompt.no_match", &[]));
        } else {
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .show(ui, |ui| {
                    for prompt in matches {
                        let tags = if prompt.tags.is_empty() {
                            String::new()
                        } else {
                            format!("   #{}", prompt.tags.join(" #"))
                        };
                        let hit = ui
                            .selectable_label(false, egui::RichText::new(&prompt.title).strong())
                            .on_hover_text(&prompt.body);
                        ui.add(egui::Label::new(egui::RichText::new(tags).small().weak()));
                        if hit.clicked() {
                            self.selected = Some(prompt.id.clone());
                            self.params.clear();
                            self.confirm_delete = false;
                        }
                    }
                });
        }
    }

    /// 상세: 파라미터 채우기 + 미리보기 + 삽입/편집/삭제.
    fn detail_view(
        &mut self,
        ui: &mut egui::Ui,
        prompt: &Prompt,
        catalog: &i18n::Catalog,
    ) -> Option<PromptPaletteAction> {
        let mut action = None;
        ui.horizontal(|ui| {
            if ui.button(catalog.t("prompt.back", &[])).clicked() {
                self.selected = None;
                self.confirm_delete = false;
                self.focus_search = true; // 목록 복귀 시 검색창 재포커스(PR-2 리뷰 Low).
            }
            ui.strong(&prompt.title);
        });
        ui.separator();
        let names = prompt.params();
        if names.is_empty() {
            ui.weak(catalog.t("prompt.no_params", &[]));
        } else {
            egui::Grid::new("prompt_params")
                .num_columns(2)
                .show(ui, |ui| {
                    for name in &names {
                        ui.monospace(format!("{{{{{name}}}}}"));
                        ui.text_edit_singleline(self.params.entry(name.clone()).or_default());
                        ui.end_row();
                    }
                });
        }
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(catalog.t("prompt.preview", &[]))
                .small()
                .weak(),
        );
        let rendered = crate::prompt_library::render(&prompt.body, &self.params);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(&rendered).monospace()).wrap());
        });
        ui.add_space(8.0);
        let ready = names
            .iter()
            .all(|n| self.params.get(n).is_some_and(|v| !v.trim().is_empty()));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(ready, egui::Button::new(catalog.t("prompt.insert", &[])))
                .clicked()
            {
                action = Some(PromptPaletteAction::Insert(rendered.clone()));
            }
            if ui.button(catalog.t("prompt.edit", &[])).clicked() {
                self.editing = Some(PromptDraft {
                    id: Some(prompt.id.clone()),
                    title: prompt.title.clone(),
                    body: prompt.body.clone(),
                    tags: prompt.tags.join(" "),
                });
                self.confirm_delete = false;
            }
            if !self.confirm_delete && ui.button(catalog.t("prompt.delete", &[])).clicked() {
                self.confirm_delete = true;
            }
        });
        if self.confirm_delete {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    catalog.t("prompt.delete_confirm", &[]),
                );
                if ui.button(catalog.t("prompt.delete_yes", &[])).clicked() {
                    action = Some(PromptPaletteAction::Delete(prompt.id.clone()));
                    self.selected = None;
                    self.confirm_delete = false;
                }
                if ui.button(catalog.t("prompt.cancel", &[])).clicked() {
                    self.confirm_delete = false;
                }
            });
        } else if !ready {
            ui.weak(catalog.t("prompt.fill_params", &[]));
        }
        action
    }

    /// 편집 폼: 제목·태그·본문 입력 + 저장/취소. `library`는 신규 저장 시 유일 id 생성용.
    fn edit_form(
        &mut self,
        ui: &mut egui::Ui,
        library: &PromptLibrary,
        catalog: &i18n::Catalog,
    ) -> Option<PromptPaletteAction> {
        let mut save = false;
        let mut cancel = false;
        {
            let draft = self.editing.as_mut().expect("editing.is_some() checked");
            ui.horizontal(|ui| {
                if ui.button(catalog.t("prompt.form.cancel", &[])).clicked() {
                    cancel = true;
                }
                ui.strong(if draft.id.is_some() {
                    catalog.t("prompt.form.edit_title", &[])
                } else {
                    catalog.t("prompt.form.new_title", &[])
                });
            });
            ui.separator();
            egui::Grid::new("prompt_edit")
                .num_columns(2)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    ui.label(catalog.t("prompt.form.title", &[]));
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.title)
                            .desired_width(f32::INFINITY)
                            .hint_text(catalog.t("prompt.form.title_hint", &[])),
                    );
                    ui.end_row();
                    ui.label(catalog.t("prompt.form.tags", &[]));
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.tags)
                            .desired_width(f32::INFINITY)
                            .hint_text(catalog.t("prompt.form.tags_hint", &[])),
                    );
                    ui.end_row();
                });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(catalog.t("prompt.form.body", &[]))
                    .small()
                    .weak(),
            );
            ui.add(
                egui::TextEdit::multiline(&mut draft.body)
                    .desired_rows(7)
                    .desired_width(f32::INFINITY)
                    .code_editor()
                    .hint_text(catalog.t("prompt.form.body_hint", &[])),
            );
            let names = crate::prompt_library::param_names(&draft.body);
            if !names.is_empty() {
                ui.label(
                    egui::RichText::new(
                        catalog.t("prompt.form.params", &[("names", &names.join(", "))]),
                    )
                    .small()
                    .weak(),
                );
            }
            ui.add_space(8.0);
            let can_save = !draft.title.trim().is_empty() && !draft.body.trim().is_empty();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        can_save,
                        egui::Button::new(catalog.t("prompt.form.save", &[])),
                    )
                    .clicked()
                {
                    save = true;
                }
                if !can_save {
                    ui.weak(catalog.t("prompt.form.fill", &[]));
                }
            });
        }
        if cancel {
            self.editing = None;
            return None;
        }
        if save {
            let draft = self.editing.take().expect("editing.is_some() checked");
            let id = draft.id.unwrap_or_else(|| library.fresh_id(&draft.title));
            // 저장 후 목록으로 — 저장된 항목을 목록에서 확인한다.
            self.selected = None;
            return Some(PromptPaletteAction::Upsert(Prompt {
                id,
                title: draft.title.trim().to_owned(),
                body: draft.body,
                tags: parse_tags(&draft.tags),
            }));
        }
        None
    }
}
