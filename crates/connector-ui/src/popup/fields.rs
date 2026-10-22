use super::{INPUT_HEIGHT, palette};

/// Same height, border and type scale as a shared text input.
pub fn choice_input<R>(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    selected: &str,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<Option<R>> {
    ui.scope(|ui| {
        let colors = palette(ui);
        ui.spacing_mut().interact_size.y = INPUT_HEIGHT;
        ui.spacing_mut().button_padding = egui::vec2(10.0, 8.0);
        let widgets = &mut ui.visuals_mut().widgets;
        for visual in [
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
        ] {
            visual.bg_fill = colors.input;
            visual.weak_bg_fill = colors.input;
            visual.bg_stroke = egui::Stroke::new(1.0, colors.border);
            visual.corner_radius = egui::CornerRadius::same(3);
            visual.expansion = 0.0;
        }
        let response = egui::ComboBox::from_id_salt(id)
            .selected_text(egui::RichText::new(selected).size(13.0))
            .width(ui.available_width())
            .show_ui(ui, contents);
        // Popup handles closing before returning, but leaves Esc in the input
        // batch. A chooser rendered this pass owns it ahead of its parent modal.
        if response.inner.is_some() {
            ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        }
        response
    })
    .inner
}

/// A complete input control, not just a preferred size on the parent Ui.
pub fn text_input(
    ui: &mut egui::Ui,
    draft: &mut dyn egui::TextBuffer,
    hint: &str,
) -> egui::Response {
    ui.scope(|ui| {
        let colors = palette(ui);
        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        let widgets = &mut ui.visuals_mut().widgets;
        for visuals in [&mut widgets.inactive, &mut widgets.hovered] {
            visuals.bg_stroke = egui::Stroke::new(1.0, colors.border);
            visuals.corner_radius = egui::CornerRadius::same(3);
            visuals.expansion = 0.0;
        }
        widgets.active.corner_radius = egui::CornerRadius::same(3);
        widgets.active.expansion = 0.0;
        ui.visuals_mut().selection.stroke = egui::Stroke::new(1.0, colors.accent);
        ui.add_sized(
            [ui.available_width(), INPUT_HEIGHT],
            egui::TextEdit::singleline(draft)
                .font(egui::FontId::proportional(13.0))
                .margin(egui::Margin::symmetric(10, 8))
                .background_color(colors.input)
                .text_color(colors.text)
                .vertical_align(egui::Align::Center)
                .hint_text(hint),
        )
    })
    .inner
}

/// A bounded editor projection: egui sees masks, while character-index edits
/// update the caller's secret. Undo/previous-text snapshots never receive values.
pub struct MaskedSecretBuffer<'a> {
    value: &'a mut String,
    mask: String,
    max_bytes: usize,
    rejected: bool,
    backup: Option<SecretBackup>,
}
struct SecretBackup(String);
fn wipe(text: &mut String) {
    unsafe {
        let bytes = text.as_mut_vec();
        for index in 0..bytes.capacity() {
            std::ptr::write_volatile(bytes.as_mut_ptr().add(index), 0);
        }
        bytes.set_len(0);
    }
}
impl Drop for SecretBackup {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}
impl<'a> MaskedSecretBuffer<'a> {
    pub fn new(value: &'a mut String, max_bytes: usize) -> Self {
        Self {
            mask: "•".repeat(value.chars().count()),
            value,
            max_bytes,
            rejected: false,
            backup: None,
        }
    }
    fn begin_edit(&mut self) {
        if self.backup.is_none() {
            self.backup = Some(SecretBackup(self.value.clone()));
        }
    }
    fn rebuild(&mut self) {
        self.mask = "•".repeat(self.value.chars().count());
    }
    /// Rejected replacement restores the entire value, including deleted selection.
    pub fn finish(self) -> bool {
        self.rejected
    }
}
impl Drop for MaskedSecretBuffer<'_> {
    fn drop(&mut self) {
        if self.rejected
            && let Some(backup) = &mut self.backup
        {
            wipe(self.value);
            *self.value = std::mem::take(&mut backup.0);
        }
    }
}
impl egui::TextBuffer for MaskedSecretBuffer<'_> {
    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<MaskedSecretBuffer<'static>>()
    }
    fn is_mutable(&self) -> bool {
        true
    }
    fn as_str(&self) -> &str {
        &self.mask
    }
    fn insert_text(&mut self, text: &str, index: egui::text::CharIndex) -> usize {
        if self.rejected
            || text.contains('\0')
            || text.len() > self.max_bytes.saturating_sub(self.value.len())
        {
            self.rejected = true;
            return 0;
        }
        self.begin_edit();
        let count = self.value.insert_text(text, index);
        self.rebuild();
        count
    }
    fn delete_char_range(&mut self, range: std::ops::Range<egui::text::CharIndex>) {
        if self.rejected {
            return;
        }
        self.begin_edit();
        self.value.delete_char_range(range);
        self.rebuild();
    }
    // Undo is disabled. A mask snapshot must never replace the actual value.
    fn replace_with(&mut self, _: &str) {}
    fn take(&mut self) -> String {
        self.mask.clone()
    }
}

/// Shared secret field. Callers clear the secret and widget state on close.
pub fn secret_input(
    ui: &mut egui::Ui,
    draft: &mut MaskedSecretBuffer<'_>,
    id: egui::Id,
    hint: &str,
    multiline: bool,
) -> egui::Response {
    // Undo carries masks only; clear it on both sides so mask edits cannot undo
    // a secret edit. This does not claim to wipe OS/input-event memory.
    let clear_undo = |ctx: &egui::Context| {
        if let Some(mut state) = egui::text_edit::TextEditState::load(ctx, id) {
            state.clear_undoer();
        }
    };
    clear_undo(ui.ctx());
    let response = ui
        .scope(|ui| {
            let colors = palette(ui);
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            let widgets = &mut ui.visuals_mut().widgets;
            for visual in [
                &mut widgets.inactive,
                &mut widgets.hovered,
                &mut widgets.active,
            ] {
                visual.bg_stroke = egui::Stroke::new(1.0, colors.border);
                visual.corner_radius = egui::CornerRadius::same(3);
                visual.expansion = 0.0;
            }
            let edit = if multiline {
                egui::TextEdit::multiline(draft).desired_rows(3)
            } else {
                egui::TextEdit::singleline(draft)
            };
            ui.add_sized(
                [
                    ui.available_width(),
                    if multiline { 80.0 } else { INPUT_HEIGHT },
                ],
                edit.id(id)
                    .password(true)
                    .font(egui::FontId::proportional(13.0))
                    .margin(egui::Margin::symmetric(10, 8))
                    .background_color(colors.input)
                    .text_color(colors.text)
                    .hint_text(hint),
            )
        })
        .inner;
    clear_undo(ui.ctx());
    response
}

pub struct PathInputResponse {
    pub input: egui::Response,
    pub browse: egui::Response,
}

/// Same input and action components, with the path occupying the remaining row.
pub fn path_input(
    ui: &mut egui::Ui,
    draft: &mut dyn egui::TextBuffer,
    browse_label: &str,
) -> PathInputResponse {
    let button_width = super::actions::button_width(ui, browse_label);
    if ui.available_width() < button_width + 7.0 + 24.0 {
        let input = text_input(ui, draft, "");
        let browse = ui
            .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                super::action_button(ui, browse_label, super::ActionTone::Secondary, true)
            })
            .inner;
        return PathInputResponse { input, browse };
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        let input_width = (ui.available_width() - button_width - 7.0).max(24.0);
        let input = ui
            .allocate_ui_with_layout(
                egui::vec2(input_width, INPUT_HEIGHT),
                egui::Layout::top_down(egui::Align::Min),
                |ui| text_input(ui, draft, ""),
            )
            .inner;
        let browse = super::action_button(ui, browse_label, super::ActionTone::Secondary, true);
        PathInputResponse { input, browse }
    })
    .inner
}

pub fn field<T>(
    ui: &mut egui::Ui,
    label: &str,
    hint: Option<&str>,
    control: impl FnOnce(&mut egui::Ui) -> T,
) -> T {
    let colors = palette(ui);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(
            egui::RichText::new(label)
                .size(12.0)
                .strong()
                .line_height(Some(17.4))
                .color(colors.text),
        );
        ui.add_space(7.0);
        let result = control(ui);
        if let Some(hint) = hint {
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(hint)
                        .size(11.0)
                        .line_height(Some(16.0))
                        .color(colors.muted),
                )
                .wrap(),
            );
        }
        result
    })
    .inner
}

pub fn segmented_choice<T: Copy + Eq, const N: usize>(
    ui: &mut egui::Ui,
    selected: &mut T,
    choices: [(T, &str); N],
) {
    let colors = palette(ui);
    egui::Frame::NONE
        .fill(colors.input)
        .stroke(egui::Stroke::new(1.0, colors.line))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(3)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                ui.spacing_mut().interact_size.y = 31.0;
                let width = ((ui.available_width() - 3.0 * N.saturating_sub(1) as f32)
                    / N.max(1) as f32)
                    .max(0.0);
                for (value, label) in choices {
                    let is_selected = *selected == value;
                    let response = ui.add_sized(
                        [width, 31.0],
                        egui::Button::new(egui::RichText::new(label).size(13.0).color(
                            if is_selected {
                                colors.text
                            } else {
                                colors.muted
                            },
                        ))
                        .wrap()
                        .selected(is_selected)
                        .fill(if is_selected {
                            colors.button
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .stroke(egui::Stroke::new(
                            1.0,
                            if is_selected {
                                colors.line
                            } else {
                                egui::Color32::TRANSPARENT
                            },
                        ))
                        .corner_radius(egui::CornerRadius::same(2)),
                    );
                    if response.has_focus() {
                        ui.painter().rect_stroke(
                            response.rect,
                            2,
                            egui::Stroke::new(2.0, colors.accent),
                            egui::StrokeKind::Inside,
                        );
                    } else if response.hovered() {
                        ui.painter().rect_stroke(
                            response.rect,
                            2,
                            egui::Stroke::new(1.0, colors.line),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if response.clicked() {
                        *selected = value;
                    }
                }
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_input_removes_undo_and_uses_shared_dimensions() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("bounded-secret-field");
        let cursor = egui::text::CCursorRange::one(egui::text::CCursor::new(0));
        let mut state = egui::text_edit::TextEditState::default();
        let mut undoer = state.undoer();
        undoer.feed_state(0.0, &(cursor, "old-secret".into()));
        undoer.feed_state(1.0, &(cursor, "new-secret".into()));
        state.set_undoer(undoer);
        state.store(&ctx, id);
        let mut draft = "test-secret".to_owned();
        let mut height = 0.0;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let mut buffer = MaskedSecretBuffer::new(&mut draft, 256);
                height = secret_input(ui, &mut buffer, id, "Secret", false)
                    .rect
                    .height();
            });
        })
        .drop_without_applying_deltas();
        assert_eq!(height, INPUT_HEIGHT);
        let stored = egui::text_edit::TextEditState::load(&ctx, id).unwrap();
        assert!(!stored.undoer().has_undo(&(cursor, draft)));
    }

    #[test]
    fn secret_projection_masks_multilingual_edits_and_rolls_back_rejected_selection() {
        use egui::TextBuffer;
        let mut value = "한a😀글".to_owned();
        {
            let mut buffer = MaskedSecretBuffer::new(&mut value, 16);
            assert_eq!(buffer.as_str(), "••••");
            buffer.delete_char_range(egui::text::CharIndex(1)..egui::text::CharIndex(3));
            assert_eq!(buffer.insert_text("é", egui::text::CharIndex(1)), 1);
            assert_eq!(buffer.as_str(), "•••");
            assert!(!buffer.finish());
        }
        assert_eq!(value, "한é글");
        {
            let mut buffer = MaskedSecretBuffer::new(&mut value, 16);
            buffer.delete_char_range(egui::text::CharIndex(0)..egui::text::CharIndex(3));
            assert_eq!(
                buffer.insert_text(&"x".repeat(17), egui::text::CharIndex(0)),
                0
            );
            assert!(buffer.finish());
        }
        assert_eq!(value, "한é글");
    }
}
