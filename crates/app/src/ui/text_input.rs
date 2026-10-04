//! Shared bounded undo policy for host-owned text fields.

pub(crate) const TEXT_EDIT_MAX_UNDOS: usize = 8;

/// Initialize once, before TextEdit (and before callers restore a caret). A fixed marker
/// avoids cloning undo history during stable frames. Existing cursor state is retained.
/// Call `forget_bounded_text_state` when the same field ID receives different content.
pub(crate) fn initialize_bounded_undo(ctx: &egui::Context, id: egui::Id) {
    let marker = id.with("bounded_undo_initialized");
    if ctx
        .data_mut(|data| data.get_temp::<bool>(marker))
        .unwrap_or(false)
    {
        return;
    }
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state.set_undoer(egui::util::undoer::Undoer::with_settings(
        egui::util::undoer::Settings {
            max_undos: TEXT_EDIT_MAX_UNDOS,
            ..Default::default()
        },
    ));
    state.store(ctx, id);
    ctx.data_mut(|data| data.insert_temp(marker, true));
}

pub(crate) fn forget_bounded_text_state(ctx: &egui::Context, id: egui::Id) {
    ctx.data_mut(|data| {
        data.remove::<egui::text_edit::TextEditState>(id);
        data.remove::<bool>(id.with("bounded_undo_initialized"));
    });
}

/// Byte budget adapter for TextEdit. Rejected-input recovery is performed by the field helper.
pub(crate) struct BoundedTextBuffer<'a> {
    pub(crate) text: &'a mut String,
    pub(crate) max_bytes: usize,
    pub(crate) rejected: &'a mut bool,
}
impl egui::TextBuffer for BoundedTextBuffer<'_> {
    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<BoundedTextBuffer<'static>>()
    }
    fn is_mutable(&self) -> bool {
        true
    }
    fn as_str(&self) -> &str {
        self.text
    }
    fn insert_text(&mut self, text: &str, index: egui::text::CharIndex) -> usize {
        if text.len() > self.max_bytes.saturating_sub(self.text.len()) {
            *self.rejected = true;
            return 0;
        }
        self.text.insert_text(text, index)
    }
    fn delete_char_range(&mut self, range: std::ops::Range<egui::text::CharIndex>) {
        self.text.delete_char_range(range);
    }
    fn replace_with(&mut self, text: &str) {
        if text.len() > self.max_bytes {
            *self.rejected = true;
        } else {
            self.text.clear();
            self.text.push_str(text);
        }
    }
    fn take(&mut self) -> String {
        std::mem::take(self.text)
    }
}

/// TextEdit deletes the selection/preedit before inserting a paste or IME commit. Save an
/// overflow-only rollback snapshot so refused insertion cannot erase that original text.
/// Stable frames and ordinary input neither clone the body nor count all its characters.
pub(crate) fn bounded_edit(
    ui: &mut egui::Ui,
    text: &mut String,
    max_bytes: usize,
    id: egui::Id,
    hint: &str,
    multiline: bool,
) -> (egui::Response, bool) {
    bounded_edit_with_style(
        ui,
        text,
        max_bytes,
        id,
        hint,
        if multiline {
            BoundedEditStyle::Multiline {
                rows: 7,
                code_editor: true,
            }
        } else {
            BoundedEditStyle::SingleLine
        },
    )
}

#[derive(Clone, Copy)]
pub(crate) enum BoundedEditStyle {
    SingleLine,
    Multiline { rows: usize, code_editor: bool },
    WindowEditor { height: f32 },
}

pub(crate) fn bounded_edit_with_style(
    ui: &mut egui::Ui,
    text: &mut String,
    max_bytes: usize,
    id: egui::Id,
    hint: &str,
    style: BoundedEditStyle,
) -> (egui::Response, bool) {
    let multiline = !matches!(style, BoundedEditStyle::SingleLine);
    initialize_bounded_undo(ui.ctx(), id);
    let incoming = ui.input(|input| {
        input
            .events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Text(text)
                | egui::Event::Paste(text)
                | egui::Event::Ime(
                    egui::ImeEvent::Preedit { text, .. } | egui::ImeEvent::Commit(text),
                ) => Some(text.len()),
                egui::Event::Key {
                    key: egui::Key::Enter | egui::Key::Tab,
                    pressed: true,
                    ..
                } if multiline => Some(1),
                _ => None,
            })
            .fold(0usize, usize::saturating_add)
    });
    let undo = ui.memory(|memory| memory.has_focus(id)) && ui.input(|input| input.events.iter().any(|event|
        matches!(event, egui::Event::Key { key: egui::Key::Z | egui::Key::Y, pressed: true, modifiers, .. } if modifiers.command)));
    let may_receive_input =
        ui.memory(|memory| memory.has_focus(id)) || ui.input(|input| input.pointer.any_pressed());
    let backup = (may_receive_input && (incoming > max_bytes.saturating_sub(text.len()) || undo))
        .then(|| text.clone());
    let state = backup
        .as_ref()
        .and_then(|_| egui::TextEdit::load_state(ui.ctx(), id));
    let undo_history = state.as_ref().map(|state| state.undoer());
    let mut rejected = false;
    let mut buffer = BoundedTextBuffer {
        text,
        max_bytes,
        rejected: &mut rejected,
    };
    let edit = match style {
        BoundedEditStyle::Multiline { rows, code_editor } => {
            let edit = egui::TextEdit::multiline(&mut buffer).desired_rows(rows);
            if code_editor {
                edit.code_editor()
            } else {
                edit
            }
        }
        BoundedEditStyle::SingleLine => egui::TextEdit::singleline(&mut buffer)
            .font(egui::FontId::proportional(13.0))
            .margin(egui::Margin::symmetric(10, 8))
            .min_size(egui::vec2(0.0, 36.0)),
        BoundedEditStyle::WindowEditor { height } => egui::TextEdit::multiline(&mut buffer)
            .desired_rows(1)
            .min_size(egui::vec2(0.0, height))
            .font(egui::FontId::proportional(13.0))
            .margin(egui::Margin::symmetric(10, 8)),
    };
    let mut response = ui.add(edit.id(id).hint_text(hint).desired_width(f32::INFINITY));
    if rejected {
        if let Some(original) = backup {
            *text = original;
        }
        if let Some(mut state) = state {
            // The shared egui undoer may have observed the failed replacement; restore the
            // overflow-only snapshot so both the previous selection and undo history survive.
            if let Some(history) = undo_history {
                state.set_undoer(history);
            }
            state.store(ui.ctx(), id);
        }
        response.mark_changed();
        ui.ctx().request_repaint();
    }
    (response, rejected)
}
