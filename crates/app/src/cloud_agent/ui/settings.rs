//! Presentation-only state. Every effect still belongs to the CloudAgent controller.
use super::layout;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(super) enum Tab {
    #[default]
    Connections,
    Permissions,
    Work,
    Advanced,
}
impl Tab {
    pub(super) const ALL: [Self; 4] = [
        Self::Connections,
        Self::Permissions,
        Self::Work,
        Self::Advanced,
    ];
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Connections => "cloud.ui.connections",
            Self::Permissions => "cloud.ui.permissions",
            Self::Work => "cloud.ui.work",
            Self::Advanced => "cloud.ui.advanced",
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(super) enum WorkKind {
    #[default]
    Runs,
    Answers,
    Receipts,
}
#[derive(Clone, Default)]
pub(super) struct SettingsState {
    pub(super) tab: Tab,
    pub(super) work_kind: WorkKind,
    pub(super) selected: Option<String>,
    pub(super) rename: String,
    pub(super) editing: bool,
    pub(super) adding: bool,
    pub(super) manual: bool,
    pub(super) server_info: bool,
    pub(super) approval: Option<String>,
    pub(super) manual_name: String,
    pub(super) manual_input_scope: bool,
    pub(super) profile: Option<usize>,
    pub(super) profile_editing: bool,
    pub(super) notice: Option<(&'static str, Option<String>)>,
}
pub(super) fn state_id() -> egui::Id {
    egui::Id::new("cloud-management-settings")
}
pub(super) fn state(ui: &egui::Ui) -> SettingsState {
    ui.ctx()
        .data_mut(|data| data.get_temp(state_id()).unwrap_or_default())
}
pub(super) fn store(ui: &egui::Ui, state: SettingsState) {
    ui.ctx()
        .data_mut(|data| data.insert_temp(state_id(), state));
}
pub(super) fn result_notice(
    state: &mut SettingsState,
    result: Result<(), String>,
    success: &'static str,
) {
    state.notice = Some(match result {
        Ok(()) => (success, None),
        Err(code) => ("cloud.management_failed", Some(code)),
    });
}
pub(super) fn notice(ui: &mut egui::Ui, catalog: &i18n::Catalog, state: &SettingsState) {
    let message = catalog.t(
        state
            .notice
            .as_ref()
            .map_or("cloud.ui.feedback", |(key, _)| *key),
        &[],
    );
    let response = ui
        .add(
            egui::Label::new(
                egui::RichText::new(&message)
                    .size(12.0)
                    .color(layout::secondary(ui)),
            )
            .truncate(),
        )
        .on_hover_text(&message);
    if let Some((_, Some(code))) = &state.notice {
        response.on_hover_text(code);
    }
}
