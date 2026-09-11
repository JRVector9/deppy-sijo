//! 초안의 입력 위젯 기록을 추적해 초안과 같은 수명으로 정리한다.

#[derive(Default)]
pub(super) struct DraftTextEditState {
    tracked: Option<(egui::Context, egui::Id)>,
}

impl DraftTextEditState {
    pub(super) fn track(&mut self, ctx: &egui::Context, id: egui::Id) {
        if self
            .tracked
            .as_ref()
            .is_some_and(|(_, tracked)| *tracked == id)
        {
            return;
        }
        self.clear();
        self.tracked = Some((ctx.clone(), id));
    }

    pub(super) fn clear(&mut self) {
        let Some((ctx, id)) = self.tracked.take() else {
            return;
        };
        if let Some(mut state) = egui::text_edit::TextEditState::load(&ctx, id) {
            // 복제된 상태도 같은 실행 취소 버퍼를 공유하므로 먼저 함께 비운다.
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

#[cfg(test)]
pub(super) fn recorded_fake_input() -> (
    DraftTextEditState,
    egui::Context,
    egui::Id,
    egui::text_edit::TextEditState,
) {
    let ctx = egui::Context::default();
    let id = egui::Id::new("pr188-fake-input");
    let cursor = egui::text::CCursorRange::one(egui::text::CCursor::new(0));
    let mut state = egui::text_edit::TextEditState::default();
    let mut undoer = state.undoer();
    undoer.feed_state(0.0, &(cursor, "fake-review-secret".into()));
    state.set_undoer(undoer);
    state.clone().store(&ctx, id);
    let mut tracked = DraftTextEditState::default();
    tracked.track(&ctx, id);
    (tracked, ctx, id, state)
}
