//! Keep a sensitive draft's widget records within the draft's lifetime.

#[derive(Default)]
pub(super) struct DraftTextEditState {
    tracked: Option<(egui::Context, egui::Id)>,
}

impl DraftTextEditState {
    pub(super) fn track(&mut self, ctx: &egui::Context, id: egui::Id) {
        if self
            .tracked
            .as_ref()
            .is_some_and(|(tracked_ctx, tracked_id)| tracked_ctx == ctx && *tracked_id == id)
        {
            return;
        }
        self.clear();
        self.tracked = Some((ctx.clone(), id));
    }

    fn clear(&mut self) {
        let Some((ctx, id)) = self.tracked.take() else {
            return;
        };
        if let Some(mut state) = egui::text_edit::TextEditState::load(&ctx, id) {
            // Cloned TextEditState values share this undo buffer. Clear them
            // before removing the context's record.
            state.clear_undoer();
        }
        ctx.data_mut(|data| data.remove::<egui::text_edit::TextEditState>(id));
        ctx.memory_mut(|memory| memory.surrender_focus(id));
    }
}

impl Drop for DraftTextEditState {
    fn drop(&mut self) {
        self.clear();
    }
}
