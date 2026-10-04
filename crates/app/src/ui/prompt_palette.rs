//! 프롬프트 라이브러리 팔레트 (기능2, leaf).
//!
//! 저장된 프롬프트를 검색·선택해 `{{param}}`를 채워 **컴포저에 삽입**하고(PR-2), 프롬프트를
//! **저장·편집·삭제**한다(PR-3). leaf 경계를 지켜 egui 상태만 소유하고, 실제 삽입·라이브러리
//! 변경은 App이 반환 intent(`Insert`/`Upsert`/`Delete`)를 받아 수행한다 — 삭제·저장은 App이
//! 파일에 영속화한다.

use std::collections::BTreeMap;

#[cfg(test)]
use super::text_input::BoundedTextBuffer;
use super::text_input::bounded_edit;

use crate::prompt_library::{
    PROMPT_BODY_MAX_BYTES, PROMPT_MAX_TAGS, PROMPT_PARAM_VALUE_MAX_BYTES,
    PROMPT_PARAM_VALUES_MAX_BYTES, PROMPT_QUERY_MAX_BYTES, PROMPT_TAG_MAX_BYTES,
    PROMPT_TITLE_MAX_BYTES, Prompt, PromptLibrary, PromptLibraryError, PromptSearchCache,
    param_names_bounded, parse_tags, render_bounded,
};

const PREVIEW_MAX_BYTES: usize = 16 * 1024;
const TAG_INPUT_MAX_BYTES: usize = PROMPT_MAX_TAGS * (PROMPT_TAG_MAX_BYTES + 1);

#[derive(Default)]
struct PreparedPrompt {
    key: Option<(u64, String)>,
    names: Vec<String>,
    error: Option<PromptLibraryError>,
    rendered: Option<String>,
    dirty: bool,
    #[cfg(test)]
    preparations: usize,
    #[cfg(test)]
    expansions: usize,
}
impl PreparedPrompt {
    fn prepare(&mut self, prompt: &Prompt, revision: u64, params: &mut BTreeMap<String, String>) {
        if self
            .key
            .as_ref()
            .is_some_and(|(rev, id)| *rev == revision && id == &prompt.id)
        {
            return;
        }
        self.key = Some((revision, prompt.id.clone()));
        self.rendered = None;
        self.dirty = true;
        match param_names_bounded(&prompt.body) {
            Ok(names) => {
                params.retain(|name, _| names.contains(name));
                self.names = names;
                self.error = None;
            }
            Err(error) => {
                self.names.clear();
                params.clear();
                self.error = Some(error);
            }
        }
        #[cfg(test)]
        {
            self.preparations += 1;
        }
    }
    fn expand(&mut self, body: &str, params: &BTreeMap<String, String>) {
        if !self.dirty || self.error.is_some() {
            return;
        }
        self.dirty = false;
        self.rendered = None;
        match render_bounded(body, params) {
            Ok(text) => self.rendered = Some(text),
            Err(error) => self.error = Some(error),
        }
        #[cfg(test)]
        {
            self.expansions += 1;
        }
    }
    fn inputs_changed(&mut self) {
        // Parse errors cannot be cleared by changing values; a revision change will reparse.
        if !self.names.is_empty() {
            self.error = None;
        }
        self.dirty = true;
    }
}

/// 팔레트가 App에 돌려주는 액션. 실제 composer 삽입·라이브러리 변경/영속화는 App이 한다.
pub enum PromptPaletteAction {
    /// 파라미터가 채워진 최종 텍스트를 컴포저 버퍼에 삽입한다.
    Insert(String),
    /// 프롬프트를 추가(신규)하거나 같은 id를 교체(편집)한다.
    Upsert { prompt: Prompt, creating: bool },
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
    parameter_hint: Option<Result<String, PromptLibraryError>>,
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
    read_only: bool,
    search_cache: PromptSearchCache,
    prepared: PreparedPrompt,
    input_limit: bool,
    reset_editor_state: bool,
    #[cfg(test)]
    rendered_rows: usize,
}

impl PromptPaletteUi {
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Host-side validation may reject a save after the leaf returns an intent. Keep the exact
    /// body available for correction rather than dropping the only editable copy.
    pub fn restore_rejected_prompt(&mut self, prompt: Prompt, creating: bool) {
        self.editing = Some(PromptDraft {
            id: (!creating).then_some(prompt.id),
            title: prompt.title,
            body: prompt.body,
            tags: prompt.tags.join(" "),
            parameter_hint: None,
        });
        self.open = true;
    }

    /// 팔레트를 연다(상태 초기화 + 검색창 포커스).
    pub fn open(&mut self) {
        self.open = true;
        self.focus_search = true;
        self.query.clear();
        self.selected = None;
        self.params.clear();
        self.editing = None;
        self.confirm_delete = false;
        self.prepared = PreparedPrompt::default();
        self.input_limit = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 팔레트를 닫는다(설정에서 기능을 끌 때 호출).
    pub fn close(&mut self) {
        self.open = false;
        self.prepared = PreparedPrompt::default();
        self.params.clear();
    }

    /// 팔레트를 그린다. 닫혀 있으면 아무것도 안 그리고 None. `composer_draft`는 "현재 컴포저
    /// 내용 저장"을 프리필하기 위한 활성 워크스페이스의 컴포저 입력이다. 삽입(Insert)은
    /// 팔레트를 닫고 컴포저로 돌아가지만, 저장/삭제(Upsert/Delete)는 계속 관리하도록
    /// 팔레트를 열어 둔다.
    pub fn render(
        &mut self,
        ctx: &egui::Context,
        library: &PromptLibrary,
        library_revision: u64,
        composer_draft: &str,
        catalog: &i18n::Catalog,
    ) -> Option<PromptPaletteAction> {
        if !self.open {
            return None;
        }
        let mut action = None;
        let mut open = true;
        super::popup::window(
            ctx,
            super::popup::WindowSpec {
                id: egui::Id::new("prompt_palette"),
                title: &catalog.t("prompt.title", &[]),
                subtitle: "",
                close_label: &catalog.t("popup.dismiss", &[]),
                close_enabled: true,
                default_size: egui::vec2(560.0, 560.0),
                min_size: egui::vec2(360.0, 340.0),
            },
            &mut open,
            |ui| {
                action = super::popup::window_body(ui, |ui| {
                    self.body(ui, library, library_revision, composer_draft, catalog)
                });
            },
        );
        // Esc: 편집 중이면 폼만 닫고(목록/상세로 복귀), 아니면 팔레트를 닫는다.
        if super::popup::take_window_escape(ctx, egui::Id::new("prompt_palette")) {
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
        library_revision: u64,
        composer_draft: &str,
        catalog: &i18n::Catalog,
    ) -> Option<PromptPaletteAction> {
        if self.editing.is_some() {
            return self.edit_form(ui, library, catalog);
        }
        match self.selected.as_deref().and_then(|id| library.get(id)) {
            Some(prompt) => self.detail_view(ui, prompt, library_revision, catalog),
            None => {
                self.list_view(ui, library, library_revision, composer_draft, catalog);
                None
            }
        }
    }

    /// 목록: 상단 액션(새 프롬프트/컴포저 내용 저장) + 검색 + 선택.
    fn list_view(
        &mut self,
        ui: &mut egui::Ui,
        library: &PromptLibrary,
        library_revision: u64,
        composer_draft: &str,
        catalog: &i18n::Catalog,
    ) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.read_only,
                    egui::Button::new(catalog.t("prompt.new", &[])),
                )
                .clicked()
            {
                self.editing = Some(PromptDraft::default());
                self.reset_editor_state = true;
            }
            if !composer_draft.trim().is_empty()
                && ui
                    .add_enabled(
                        !self.read_only && composer_draft.len() <= PROMPT_BODY_MAX_BYTES,
                        egui::Button::new(catalog.t("prompt.save_from_composer", &[])),
                    )
                    .on_hover_text(catalog.t("prompt.save_from_composer.hint", &[]))
                    .clicked()
            {
                self.editing = Some(PromptDraft {
                    id: None,
                    title: String::new(),
                    body: composer_draft.to_owned(),
                    tags: String::new(),
                    parameter_hint: None,
                });
                self.reset_editor_state = true;
            }
        });
        ui.separator();
        let (search, rejected) = bounded_edit(
            ui,
            &mut self.query,
            PROMPT_QUERY_MAX_BYTES,
            egui::Id::new("prompt_query"),
            &catalog.t("prompt.search", &[]),
            false,
        );
        if rejected {
            self.input_limit = true;
        } else if search.changed() {
            self.input_limit = false;
        }
        if self.focus_search {
            search.request_focus();
            self.focus_search = false;
        }
        if self.input_limit || composer_draft.len() > PROMPT_BODY_MAX_BYTES {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                catalog.t("prompt.input_limit", &[]),
            );
        }
        ui.add_space(4.0);
        let matches = match self
            .search_cache
            .update(library, library_revision, &self.query)
        {
            Ok(indices) => indices,
            Err(_) => {
                ui.weak(catalog.t("prompt.input_limit", &[]));
                return;
            }
        };
        if matches.is_empty() {
            ui.weak(catalog.t("prompt.no_match", &[]));
        } else {
            // Each result stays two non-wrapping lines so show_rows can skip all offscreen
            // title/tag formatting and text layout without caching duplicate prompt bodies.
            let row_height = (ui.text_style_height(&egui::TextStyle::Body)
                + 2.0 * ui.spacing().button_padding.y)
                .max(ui.spacing().interact_size.y)
                + ui.text_style_height(&egui::TextStyle::Small)
                + ui.spacing().item_spacing.y;
            egui::ScrollArea::vertical()
                .id_salt("prompt_results")
                .max_height(320.0)
                .show_rows(ui, row_height, matches.len(), |ui, rows| {
                    for row in rows {
                        let Some(prompt) = library.prompts.get(matches[row]) else {
                            continue;
                        };
                        #[cfg(test)]
                        {
                            self.rendered_rows += 1;
                        }
                        ui.push_id(row, |ui| {
                            let hit = ui.add(
                                egui::Button::selectable(
                                    false,
                                    egui::RichText::new(&prompt.title).strong(),
                                )
                                .truncate(),
                            );
                            // A tooltip also has a bounded layout, even for 1 MiB templates.
                            hit.clone().on_hover_ui(|ui| {
                                ui.label(preview_slice(&prompt.body));
                                if prompt.body.len() > PREVIEW_MAX_BYTES {
                                    ui.weak(catalog.t("prompt.preview_truncated", &[]));
                                }
                            });
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(prompt.tags.join(" #")).small().weak(),
                                )
                                .truncate(),
                            );
                            if hit.clicked() {
                                self.selected = Some(prompt.id.clone());
                                self.params.clear();
                                self.prepared = PreparedPrompt::default();
                                self.confirm_delete = false;
                            }
                        });
                    }
                });
        }
    }

    /// 상세: 파라미터 채우기 + 미리보기 + 삽입/편집/삭제.
    fn detail_view(
        &mut self,
        ui: &mut egui::Ui,
        prompt: &Prompt,
        library_revision: u64,
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
        let changed_prompt = !self
            .prepared
            .key
            .as_ref()
            .is_some_and(|(rev, id)| *rev == library_revision && id == &prompt.id);
        self.prepared
            .prepare(prompt, library_revision, &mut self.params);
        if changed_prompt {
            // Reuse a fixed set of IDs; parameter names must not accumulate persistent egui
            // undo histories across all saved templates. New contents cannot inherit undo.
            for index in 0..crate::prompt_library::PROMPT_PARAM_MAX_NAMES {
                let id = egui::Id::new(("prompt_param", index));
                super::text_input::forget_bounded_text_state(ui.ctx(), id);
                ui.memory_mut(|memory| memory.surrender_focus(id));
            }
        }
        if self.prepared.names.is_empty() && self.prepared.error.is_none() {
            ui.weak(catalog.t("prompt.no_params", &[]));
        } else {
            let mut changed = false;
            let mut rejected = false;
            let mut total_bytes: usize = self.params.values().map(String::len).sum();
            egui::ScrollArea::vertical()
                .id_salt("prompt_param_fields")
                .max_height(180.0)
                .show(ui, |ui| {
                    egui::Grid::new("prompt_params")
                        .num_columns(2)
                        .show(ui, |ui| {
                            for (index, name) in self.prepared.names.iter().enumerate() {
                                ui.monospace(format!("{{{{{name}}}}}"));
                                let value = self.params.entry(name.clone()).or_default();
                                let previous = value.len();
                                let budget = PROMPT_PARAM_VALUE_MAX_BYTES.min(
                                    PROMPT_PARAM_VALUES_MAX_BYTES
                                        .saturating_sub(total_bytes - previous),
                                );
                                let (response, refused) = bounded_edit(
                                    ui,
                                    value,
                                    budget,
                                    egui::Id::new(("prompt_param", index)),
                                    "",
                                    false,
                                );
                                total_bytes = total_bytes - previous + value.len();
                                changed |= response.changed();
                                rejected |= refused;
                                ui.end_row();
                            }
                        });
                });
            if changed {
                self.prepared.inputs_changed();
                self.input_limit = false;
            }
            if rejected {
                self.input_limit = true;
            }
        }
        self.prepared.expand(&prompt.body, &self.params);
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(catalog.t("prompt.preview", &[]))
                .small()
                .weak(),
        );
        if let Some(rendered) = &self.prepared.rendered {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("prompt_preview")
                    .max_height(160.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(preview_slice(rendered)).monospace(),
                            )
                            .wrap(),
                        );
                    });
            });
            if rendered.len() > PREVIEW_MAX_BYTES {
                ui.weak(catalog.t("prompt.preview_truncated", &[]));
            }
        }
        if self.prepared.error.is_some() || self.input_limit {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                catalog.t("prompt.input_limit", &[]),
            );
        }
        ui.add_space(8.0);
        let ready = self.prepared.rendered.is_some()
            && self.prepared.error.is_none()
            && self
                .prepared
                .names
                .iter()
                .all(|n| self.params.get(n).is_some_and(|v| !v.trim().is_empty()));
        ui.horizontal(|ui| {
            if ui
                .add_enabled(ready, egui::Button::new(catalog.t("prompt.insert", &[])))
                .clicked()
            {
                action = Some(PromptPaletteAction::Insert(
                    self.prepared
                        .rendered
                        .as_ref()
                        .expect("ready checked")
                        .clone(),
                ));
            }
            if ui
                .add_enabled(
                    !self.read_only,
                    egui::Button::new(catalog.t("prompt.edit", &[])),
                )
                .clicked()
            {
                self.editing = Some(PromptDraft {
                    id: Some(prompt.id.clone()),
                    title: prompt.title.clone(),
                    body: prompt.body.clone(),
                    tags: prompt.tags.join(" "),
                    parameter_hint: None,
                });
                self.reset_editor_state = true;
                self.confirm_delete = false;
            }
            if !self.confirm_delete
                && ui
                    .add_enabled(
                        !self.read_only,
                        egui::Button::new(catalog.t("prompt.delete", &[])),
                    )
                    .clicked()
            {
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
                if ui
                    .add_enabled(
                        !self.read_only,
                        egui::Button::new(catalog.t("prompt.delete_yes", &[])),
                    )
                    .clicked()
                {
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
        if self.reset_editor_state {
            for key in ["prompt_edit_title", "prompt_edit_tags", "prompt_edit_body"] {
                let id = egui::Id::new(key);
                super::text_input::forget_bounded_text_state(ui.ctx(), id);
                ui.memory_mut(|memory| memory.surrender_focus(id));
            }
            self.reset_editor_state = false;
        }
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
            let mut rejected = false;
            let mut changed = false;
            egui::Grid::new("prompt_edit")
                .num_columns(2)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    ui.label(catalog.t("prompt.form.title", &[]));
                    let (response, refused) = bounded_edit(
                        ui,
                        &mut draft.title,
                        PROMPT_TITLE_MAX_BYTES,
                        egui::Id::new("prompt_edit_title"),
                        &catalog.t("prompt.form.title_hint", &[]),
                        false,
                    );
                    rejected |= refused;
                    changed |= response.changed();
                    ui.end_row();
                    ui.label(catalog.t("prompt.form.tags", &[]));
                    let (response, refused) = bounded_edit(
                        ui,
                        &mut draft.tags,
                        TAG_INPUT_MAX_BYTES,
                        egui::Id::new("prompt_edit_tags"),
                        &catalog.t("prompt.form.tags_hint", &[]),
                        false,
                    );
                    rejected |= refused;
                    changed |= response.changed();
                    ui.end_row();
                });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(catalog.t("prompt.form.body", &[]))
                    .small()
                    .weak(),
            );
            let (response, refused) = bounded_edit(
                ui,
                &mut draft.body,
                PROMPT_BODY_MAX_BYTES,
                egui::Id::new("prompt_edit_body"),
                &catalog.t("prompt.form.body_hint", &[]),
                true,
            );
            rejected |= refused;
            changed |= response.changed();
            if response.changed() {
                draft.parameter_hint = None;
            }
            let names = draft.parameter_hint.get_or_insert_with(|| {
                param_names_bounded(&draft.body).map(|names| names.join(", "))
            });
            if let Ok(names) = names
                && !names.is_empty()
            {
                ui.label(
                    egui::RichText::new(catalog.t("prompt.form.params", &[("names", names)]))
                        .small()
                        .weak(),
                );
            }
            if changed {
                self.input_limit = false;
            }
            if rejected {
                self.input_limit = true;
            }
            if names.is_err()
                || self.input_limit
                || draft.title.len() > PROMPT_TITLE_MAX_BYTES
                || draft.body.len() > PROMPT_BODY_MAX_BYTES
                || draft.tags.len() > TAG_INPUT_MAX_BYTES
            {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    catalog.t("prompt.input_limit", &[]),
                );
            }
            ui.add_space(8.0);
            let can_save = !draft.title.trim().is_empty()
                && !draft.body.trim().is_empty()
                && draft.title.len() <= PROMPT_TITLE_MAX_BYTES
                && draft.body.len() <= PROMPT_BODY_MAX_BYTES
                && draft.tags.len() <= TAG_INPUT_MAX_BYTES;
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        can_save && !self.read_only,
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
            let creating = draft.id.is_none();
            let id = draft.id.unwrap_or_else(|| library.fresh_id(&draft.title));
            // 저장 후 목록으로 — 저장된 항목을 목록에서 확인한다.
            self.selected = None;
            return Some(PromptPaletteAction::Upsert {
                prompt: Prompt {
                    id,
                    title: draft.title.trim().to_owned(),
                    body: draft.body,
                    tags: parse_tags(&draft.tags),
                },
                creating,
            });
        }
        None
    }
}

fn preview_slice(text: &str) -> &str {
    &text[..text.floor_char_boundary(PREVIEW_MAX_BYTES.min(text.len()))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr15_rejected_new_creation_keeps_creation_identity_and_exact_body() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let library = PromptLibrary { prompts: vec![] };
        let palette = PromptPaletteUi {
            open: true,
            editing: Some(PromptDraft {
                title: "A".repeat(257),
                body: "KEEP 한글".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (PromptPaletteUi, Option<Prompt>)| {
                if state.0.editing.is_some()
                    && let Some(PromptPaletteAction::Upsert { prompt, creating }) =
                        state.0.edit_form(ui, &library, &catalog)
                {
                    assert!(creating);
                    state.1 = Some(prompt);
                }
            },
            (palette, None),
        );
        harness.run();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        harness
            .get_by_label(&catalog.t("prompt.form.save", &[]))
            .click();
        harness.run();
        let prompt = harness.state_mut().1.take().expect("actual form save");
        harness.state_mut().0.restore_rejected_prompt(prompt, true);
        let draft = harness.state().0.editing.as_ref().unwrap();
        assert!(
            draft.id.is_none(),
            "a rejected creation must regenerate its identity after correction"
        );
        assert_eq!(draft.body, "KEEP 한글");
        harness.state_mut().0.editing.as_mut().unwrap().title = "Corrected".into();
        harness.run();
        harness
            .get_by_label(&catalog.t("prompt.form.save", &[]))
            .click();
        harness.run();
        let corrected = harness.state_mut().1.take().expect("corrected form save");
        assert_eq!(corrected.id, "corrected");
        assert_eq!(corrected.body, "KEEP 한글");
        assert!(
            PromptLibrary { prompts: vec![] }
                .validate_upsert(&corrected)
                .is_ok()
        );
    }

    #[test]
    fn pr4_text_buffer_rejects_whole_multibyte_input_preserving_original() {
        use egui::TextBuffer;
        let mut original = "KEEP".to_owned();
        let mut rejected = false;
        let mut buffer = BoundedTextBuffer {
            text: &mut original,
            max_bytes: 7,
            rejected: &mut rejected,
        };
        assert_eq!(buffer.insert_text("가나", egui::text::CharIndex(4)), 0);
        assert_eq!(original, "KEEP");
        assert!(rejected);
    }

    #[test]
    fn pr4_text_buffer_exact_utf8_budget_and_replacement_are_atomic() {
        use egui::TextBuffer;
        let mut text = "ab".to_owned();
        let mut rejected = false;
        let mut buffer = BoundedTextBuffer {
            text: &mut text,
            max_bytes: 5,
            rejected: &mut rejected,
        };
        assert_eq!(buffer.insert_text("가", egui::text::CharIndex(1)), 1);
        assert_eq!(buffer.as_str(), "a가b");
        buffer.replace_with("한글");
        assert_eq!(buffer.as_str(), "a가b");
        buffer.delete_char_range(egui::text::CharIndex(0)..egui::text::CharIndex(1));
        assert_eq!(buffer.as_str(), "가b");
        assert!(rejected);
        let mut existing = "preserve oversized rejected form".to_owned();
        let original = existing.clone();
        let mut refused = false;
        let mut buffer = BoundedTextBuffer {
            text: &mut existing,
            max_bytes: 5,
            rejected: &mut refused,
        };
        assert_eq!(buffer.insert_text("x", egui::text::CharIndex(0)), 0);
        assert_eq!(buffer.as_str(), original);
        buffer.delete_char_range(
            egui::text::CharIndex(5)..egui::text::CharIndex(original.chars().count()),
        );
        assert_eq!(buffer.as_str(), "prese");
    }

    #[test]
    fn pr4_selected_cache_reuses_and_invalidates_names_values_revision() {
        let mut prompt = Prompt {
            id: "p".into(),
            title: "title".into(),
            body: "한글 {{x}}".into(),
            tags: vec![],
        };
        let mut prepared = PreparedPrompt::default();
        let mut params = BTreeMap::from([("x".into(), "ONE".into())]);
        for _ in 0..10 {
            prepared.prepare(&prompt, 1, &mut params);
            prepared.expand(&prompt.body, &params);
        }
        assert_eq!(prepared.preparations, 1);
        assert_eq!(prepared.expansions, 1);
        assert_eq!(prepared.rendered.as_deref(), Some("한글 ONE"));
        params.insert("x".into(), "TWO".into());
        prepared.inputs_changed();
        prepared.expand(&prompt.body, &params);
        assert_eq!(prepared.rendered.as_deref(), Some("한글 TWO"));
        prompt.body = "{{y}}".into();
        prepared.prepare(&prompt, 2, &mut params);
        prepared.expand(&prompt.body, &params);
        assert_eq!(prepared.names, ["y"]);
        assert!(params.is_empty());
        assert_eq!(prepared.rendered.as_deref(), Some(""));
        prompt.body = "{{y}}".repeat(129);
        prepared.prepare(&prompt, 3, &mut params);
        params.insert("y".into(), "x".repeat(8192));
        prepared.expand(&prompt.body, &params);
        assert!(prepared.rendered.is_none());
        assert!(prepared.error.is_some());
        params.insert("y".into(), "OK".into());
        prepared.inputs_changed();
        prepared.expand(&prompt.body, &params);
        assert!(prepared.error.is_none());
        assert_eq!(prepared.rendered.as_ref().unwrap().len(), 258);
        assert!(preview_slice(&"한".repeat(6000)).len() <= PREVIEW_MAX_BYTES);
    }

    #[test]
    fn pr4_real_textedit_overflow_paste_preserves_selection_and_ime_original() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("bounded_field_test");
        let mut text = "KEEP".to_owned();
        let draw = |ui: &mut egui::Ui, text: &mut String| bounded_edit(ui, text, 7, id, "", false);
        ctx.run_ui(egui::RawInput::default(), |ui| {
            draw(ui, &mut text);
        })
        .drop_without_applying_deltas();
        ctx.memory_mut(|memory| memory.request_focus(id));
        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(egui::text::CharIndex(0)),
                egui::text::CCursor::new(egui::text::CharIndex(4)),
            )));
        state.store(&ctx, id);
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Paste("가나다".into()));
        let mut refused = false;
        ctx.run_ui(input, |ui| {
            refused = draw(ui, &mut text).1;
        })
        .drop_without_applying_deltas();
        assert!(refused);
        assert_eq!(text, "KEEP");
        let mut input = egui::RawInput::default();
        input
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Commit("가나다".into())));
        ctx.run_ui(input, |ui| {
            refused = draw(ui, &mut text).1;
        })
        .drop_without_applying_deltas();
        assert!(refused);
        assert_eq!(text, "KEEP");
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Paste("가나".into()));
        ctx.run_ui(input, |ui| {
            refused = draw(ui, &mut text).1;
        })
        .drop_without_applying_deltas();
        assert!(!refused);
        assert_eq!(text, "가나");
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        ctx.run_ui(input, |ui| {
            draw(ui, &mut text);
        })
        .drop_without_applying_deltas();
        assert_eq!(
            text, "KEEP",
            "valid Undo must survive refused paste and IME"
        );
    }

    #[test]
    fn pr4_real_textedit_newline_overflow_preserves_existing_rejected_form() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("bounded_multiline_test");
        let mut text = "KEEP".to_owned();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            bounded_edit(ui, &mut text, 3, id, "", true);
        })
        .drop_without_applying_deltas();
        ctx.memory_mut(|memory| memory.request_focus(id));
        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(1),
            )));
        state.store(&ctx, id);
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let mut refused = false;
        ctx.run_ui(input, |ui| {
            refused = bounded_edit(ui, &mut text, 3, id, "", true).1;
        })
        .drop_without_applying_deltas();
        assert!(refused);
        assert_eq!(text, "KEEP");
    }

    #[test]
    fn pr4_real_textedit_undo_history_is_bounded_and_still_restores_text() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("bounded_history_test");
        let mut text = "BASE".to_owned();
        let mut time = 0.0;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            bounded_edit(ui, &mut text, PROMPT_BODY_MAX_BYTES, id, "", true);
        })
        .drop_without_applying_deltas();
        ctx.memory_mut(|memory| memory.request_focus(id));
        let mut state = egui::TextEdit::load_state(&ctx, id).unwrap();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(4),
            )));
        state.store(&ctx, id);
        for _ in 0..20 {
            time += 2.0;
            let mut input = egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            input.events.push(egui::Event::Text("a".into()));
            ctx.run_ui(input, |ui| {
                bounded_edit(ui, &mut text, PROMPT_BODY_MAX_BYTES, id, "", true);
            })
            .drop_without_applying_deltas();
            time += 2.0;
            ctx.run_ui(
                egui::RawInput {
                    time: Some(time),
                    ..Default::default()
                },
                |ui| {
                    bounded_edit(ui, &mut text, PROMPT_BODY_MAX_BYTES, id, "", true);
                },
            )
            .drop_without_applying_deltas();
        }
        let mut undos = 0;
        for _ in 0..25 {
            let previous = text.clone();
            time += 2.0;
            let mut input = egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            input.events.push(egui::Event::Key {
                key: egui::Key::Z,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            });
            ctx.run_ui(input, |ui| {
                bounded_edit(ui, &mut text, PROMPT_BODY_MAX_BYTES, id, "", true);
            })
            .drop_without_applying_deltas();
            if text == previous {
                break;
            }
            undos += 1;
            assert!(text.starts_with("BASE"));
        }
        assert!(undos > 0);
        assert!(
            undos <= 8,
            "egui retained {undos} full snapshots instead of bounded8"
        );
    }

    #[test]
    fn pr4_palette_only_materializes_visible_result_rows() {
        let library = PromptLibrary {
            prompts: (0..1000)
                .map(|i| Prompt {
                    id: format!("p{i}"),
                    title: format!("Prompt {i}"),
                    body: "body".into(),
                    tags: vec![],
                })
                .collect(),
        };
        let ctx = egui::Context::default();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut palette = PromptPaletteUi::default();
        palette.open();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            palette.render(ui.ctx(), &library, 1, "", &catalog);
        })
        .drop_without_applying_deltas();
        assert!(palette.rendered_rows > 0);
        println!(
            "PR4 actual visible rows materialized: {} / 1000",
            palette.rendered_rows
        );
        assert!(
            palette.rendered_rows < 32,
            "rendered all {} rows instead of visible rows",
            palette.rendered_rows
        );
    }

    #[test]
    fn pr4_real_palette_new_form_cannot_undo_into_another_prompt_but_rejected_form_can_undo() {
        use egui_kittest::kittest::Queryable;
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let library = PromptLibrary {
            prompts: ["A", "B"]
                .into_iter()
                .map(|name| Prompt {
                    id: name.into(),
                    title: format!("Title {name}"),
                    body: format!("{name}_BODY"),
                    tags: vec![],
                })
                .collect(),
        };
        let mut palette = PromptPaletteUi::default();
        palette.open();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, palette: &mut PromptPaletteUi| {
                palette.render(ui.ctx(), &library, 1, "", &catalog);
            },
            palette,
        );
        let catalog = i18n::Catalog::load("en-US").unwrap();
        harness.run();
        harness.get_by_label("Title A").click();
        harness.run();
        harness.get_by_label(&catalog.t("prompt.edit", &[])).click();
        harness.run();
        assert_eq!(harness.state().editing.as_ref().unwrap().body, "A_BODY");
        let id = egui::Id::new("prompt_edit_body");
        harness.ctx.memory_mut(|memory| memory.request_focus(id));
        let mut state = egui::TextEdit::load_state(&harness.ctx, id).unwrap();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(6),
            )));
        state.store(&harness.ctx, id);
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("K".into()));
        harness.run();
        assert_eq!(harness.state().editing.as_ref().unwrap().body, "A_BODYK");
        harness
            .get_by_label(&catalog.t("prompt.form.cancel", &[]))
            .click();
        harness.run();
        harness.get_by_label(&catalog.t("prompt.back", &[])).click();
        harness.run();
        harness.get_by_label("Title B").click();
        harness.run();
        harness.get_by_label(&catalog.t("prompt.edit", &[])).click();
        harness.run();
        let id = egui::Id::new("prompt_edit_body");
        harness.ctx.memory_mut(|memory| memory.request_focus(id));
        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run();
        assert_eq!(
            harness.state().editing.as_ref().unwrap().body,
            "B_BODY",
            "a different prompt must not inherit previous editor undo"
        );
        let mut state = egui::TextEdit::load_state(&harness.ctx, id).unwrap();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(6),
            )));
        state.store(&harness.ctx, id);
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("K".into()));
        harness.run();
        assert_eq!(harness.state().editing.as_ref().unwrap().body, "B_BODYK");
        harness.state_mut().restore_rejected_prompt(
            Prompt {
                id: "B".into(),
                title: "Title B".into(),
                body: "B_BODYK".into(),
                tags: vec![],
            },
            false,
        );
        harness.run();
        harness.ctx.memory_mut(|memory| memory.request_focus(id));
        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run();
        assert_eq!(
            harness.state().editing.as_ref().unwrap().body,
            "B_BODY",
            "a rejected exact form must preserve its valid undo"
        );
    }

    #[test]
    fn pr4_real_palette_select_insert_edit_save_delete_keep_complete_unicode() {
        use egui_kittest::kittest::Queryable;
        struct State {
            palette: PromptPaletteUi,
            library: PromptLibrary,
            revision: u64,
            inserted: Option<String>,
        }
        let body = "한글".repeat(4000);
        let library = PromptLibrary {
            prompts: vec![Prompt {
                id: "p".into(),
                title: "Original".into(),
                body: body.clone(),
                tags: vec!["tag".into()],
            }],
        };
        let mut palette = PromptPaletteUi::default();
        palette.open();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut State| match state.palette.render(
                ui.ctx(),
                &state.library,
                state.revision,
                "",
                &catalog,
            ) {
                Some(PromptPaletteAction::Insert(text)) => state.inserted = Some(text),
                Some(PromptPaletteAction::Upsert { prompt, .. }) => {
                    state.library.try_upsert(prompt).unwrap();
                    state.revision += 1;
                }
                Some(PromptPaletteAction::Delete(id)) => {
                    state.library.delete(&id);
                    state.revision += 1;
                }
                None => {}
            },
            State {
                palette,
                library,
                revision: 1,
                inserted: None,
            },
        );
        let catalog = i18n::Catalog::load("en-US").unwrap();
        harness.run();
        harness.get_by_label("Original").click();
        harness.run();
        harness
            .get_by_label(&catalog.t("prompt.insert", &[]))
            .click();
        harness.run();
        assert_eq!(harness.state().inserted.as_deref(), Some(body.as_str()));
        assert!(!harness.state().palette.is_open());
        harness.state_mut().palette.open();
        harness.run();
        harness.get_by_label("Original").click();
        harness.run();
        harness.get_by_label(&catalog.t("prompt.edit", &[])).click();
        harness.run();
        let draft = harness.state_mut().palette.editing.as_mut().unwrap();
        assert_eq!(draft.body, body);
        draft.title = "수정한 제목".into();
        draft.body = "새로운 한글 본문".into();
        harness.run();
        harness
            .get_by_label(&catalog.t("prompt.form.save", &[]))
            .click();
        harness.run();
        assert_eq!(harness.state().library.prompts[0].body, "새로운 한글 본문");
        assert_eq!(harness.state().revision, 2);
        harness.get_by_label("수정한 제목").click();
        harness.run();
        harness
            .get_by_label(&catalog.t("prompt.delete", &[]))
            .click();
        harness.run();
        harness
            .get_by_label(&catalog.t("prompt.delete_yes", &[]))
            .click();
        harness.run();
        assert!(harness.state().library.prompts.is_empty());
        assert_eq!(harness.state().revision, 3);
    }

    #[test]
    fn pr3_palette_readonly_blocks_mutations_and_rejected_body_is_retained() {
        use egui_kittest::kittest::Queryable;
        let library = PromptLibrary::default_seed();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let mut palette = PromptPaletteUi::default();
        palette.open();
        palette.set_read_only(true);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (PromptPaletteUi, bool)| {
                state.1 |= state
                    .0
                    .render(ui.ctx(), &library, 1, "draft", &catalog)
                    .is_some();
            },
            (palette, false),
        );
        harness.run();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        harness.get_by_label(&catalog.t("prompt.new", &[])).click();
        harness.run();
        assert!(harness.state().0.editing.is_none());
        let prompt = Prompt {
            id: "rejected".into(),
            title: "Rejected".into(),
            body: "가나다\nKEEP EXACT".into(),
            tags: vec!["tag".into()],
        };
        harness
            .state_mut()
            .0
            .restore_rejected_prompt(prompt.clone(), false);
        harness.run();
        harness
            .get_by_label(&catalog.t("prompt.form.save", &[]))
            .click();
        harness.run();
        assert!(!harness.state().1);
        assert_eq!(
            harness.state().0.editing.as_ref().unwrap().body,
            prompt.body
        );
        assert_eq!(
            harness.state().0.editing.as_ref().unwrap().title,
            prompt.title
        );
    }

    #[test]
    fn popup_audit_palette_draft_survives_escape_owned_by_confirmation() {
        let ctx = egui::Context::default();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let library = PromptLibrary::default();
        let mut palette = PromptPaletteUi::default();
        palette.open();
        palette.editing = Some(PromptDraft {
            title: "Keep draft".into(),
            ..Default::default()
        });
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        ctx.run_ui(input, |ui| {
            crate::ui::popup::show(
                ui.ctx(),
                crate::ui::popup::PopupSpec {
                    id: egui::Id::new("palette_front_confirm"),
                    width: 400.0,
                    title: "Confirm",
                    subtitle: "",
                    close_label: "Close",
                    close_enabled: true,
                },
                |ui| {
                    ui.label("Front confirmation");
                },
            );
            assert!(
                palette
                    .render(ui.ctx(), &library, 1, "", &catalog)
                    .is_none()
            );
        })
        .drop_without_applying_deltas();
        assert!(palette.is_open());
        assert_eq!(
            palette.editing.as_ref().map(|draft| draft.title.as_str()),
            Some("Keep draft")
        );
    }

    #[test]
    fn popup_audit_front_palette_escape_returns_from_edit_then_closes() {
        let ctx = egui::Context::default();
        let catalog = i18n::Catalog::load("en-US").unwrap();
        let library = PromptLibrary::default();
        let mut palette = PromptPaletteUi::default();
        palette.open();
        palette.editing = Some(PromptDraft::default());
        for _ in 0..2 {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                palette.render(ui.ctx(), &library, 1, "", &catalog);
            })
            .drop_without_applying_deltas();
        }
        for editing in [true, false] {
            let mut input = egui::RawInput::default();
            input.events.push(egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
            ctx.run_ui(input, |ui| {
                palette.render(ui.ctx(), &library, 1, "", &catalog);
            })
            .drop_without_applying_deltas();
            assert_eq!(palette.is_open(), editing);
            assert!(palette.editing.is_none());
        }
    }
}
