//! 프롬프트 라이브러리 팔레트 (기능2 PR-2, leaf).
//!
//! 저장된 프롬프트를 검색·선택하고 `{{param}}`를 채워 **컴포저에 삽입**한다. leaf 경계를
//! 지켜 egui 상태만 소유하고, 실제 삽입(WriteInput 이전의 composer 버퍼 쓰기)은 App이
//! 반환 intent(`Insert`)를 받아 수행한다.

use std::collections::BTreeMap;

use crate::prompt_library::PromptLibrary;

/// 팔레트가 App에 돌려주는 액션. 실제 composer 삽입은 App이 한다.
pub enum PromptPaletteAction {
    /// 파라미터가 채워진 최종 텍스트를 컴포저 버퍼에 삽입한다.
    Insert(String),
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
}

impl PromptPaletteUi {
    /// 팔레트를 연다(상태 초기화 + 검색창 포커스).
    pub fn open(&mut self) {
        self.open = true;
        self.focus_search = true;
        self.query.clear();
        self.selected = None;
        self.params.clear();
    }

    /// 팔레트를 그린다. 닫혀 있으면 아무것도 안 그리고 None. 사용자가 삽입을 누르면
    /// 렌더된 텍스트를 담은 `Insert`를 돌려주고 팔레트를 닫는다.
    pub fn render(
        &mut self,
        ctx: &egui::Context,
        library: &PromptLibrary,
    ) -> Option<PromptPaletteAction> {
        if !self.open {
            return None;
        }
        let mut action = None;
        let mut open = true;
        egui::Window::new("프롬프트 라이브러리")
            .id(egui::Id::new("prompt_palette"))
            .collapsible(false)
            .resizable(true)
            .default_width(540.0)
            .open(&mut open)
            .show(ctx, |ui| {
                action = self.body(ui, library);
            });
        // Esc/닫기 버튼 또는 삽입 후 닫는다.
        self.open = open && action.is_none();
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.open = false;
        }
        action
    }

    fn body(
        &mut self,
        ui: &mut egui::Ui,
        library: &PromptLibrary,
    ) -> Option<PromptPaletteAction> {
        match self.selected.clone().and_then(|id| library.get(&id)) {
            Some(prompt) => {
                // ── 상세: 파라미터 채우기 + 미리보기 ──
                ui.horizontal(|ui| {
                    if ui.button("← 목록").clicked() {
                        self.selected = None;
                    }
                    ui.strong(&prompt.title);
                });
                ui.separator();
                let names = prompt.params();
                if names.is_empty() {
                    ui.weak("파라미터 없음");
                } else {
                    egui::Grid::new("prompt_params").num_columns(2).show(ui, |ui| {
                        for name in &names {
                            ui.monospace(format!("{{{{{name}}}}}"));
                            ui.text_edit_singleline(self.params.entry(name.clone()).or_default());
                            ui.end_row();
                        }
                    });
                }
                ui.add_space(8.0);
                ui.label(egui::RichText::new("미리보기").small().weak());
                let rendered = crate::prompt_library::render(&prompt.body, &self.params);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(&rendered).monospace())
                            .wrap(),
                    );
                });
                ui.add_space(8.0);
                let ready = names.iter().all(|n| {
                    self.params.get(n).is_some_and(|v| !v.trim().is_empty())
                });
                ui.horizontal(|ui| {
                    let insert = ui.add_enabled(ready, egui::Button::new("컴포저에 삽입 ↵"));
                    if insert.clicked() {
                        return Some(PromptPaletteAction::Insert(rendered.clone()));
                    }
                    if !ready {
                        ui.weak("모든 파라미터를 채우세요");
                    }
                    None
                })
                .inner
            }
            None => {
                // ── 목록: 검색 + 선택 ──
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("프롬프트 검색…")
                        .desired_width(f32::INFINITY),
                );
                if self.focus_search {
                    search.request_focus();
                    self.focus_search = false;
                }
                ui.add_space(4.0);
                let matches = library.search(&self.query);
                if matches.is_empty() {
                    ui.weak("일치하는 프롬프트가 없습니다.");
                } else {
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for prompt in matches {
                            let tags = if prompt.tags.is_empty() {
                                String::new()
                            } else {
                                format!("   #{}", prompt.tags.join(" #"))
                            };
                            let label = egui::RichText::new(&prompt.title).strong();
                            let hit = ui
                                .selectable_label(false, label)
                                .on_hover_text(&prompt.body);
                            ui.add(egui::Label::new(
                                egui::RichText::new(tags).small().weak(),
                            ));
                            if hit.clicked() {
                                self.selected = Some(prompt.id.clone());
                                self.params.clear();
                            }
                        }
                    });
                }
                None
            }
        }
    }
}
