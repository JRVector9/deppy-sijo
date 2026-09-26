//! Cloud replies belong to their persistent session, never its PTY input.
#[derive(Clone)]
pub struct Answer {
    pub id: String,
    pub session: String,
    pub created: i64,
    pub message: String,
}
pub fn contents(
    ui: &mut egui::Ui,
    records: &[Answer],
    session: &str,
    selected: &mut Option<String>,
    catalog: &i18n::Catalog,
) {
    let answers: Vec<_> = records.iter().filter(|r| r.session == session).collect();
    if answers.is_empty() {
        return;
    }
    let selected_id = if answers
        .iter()
        .any(|r| selected.as_deref() == Some(r.id.as_str()))
    {
        selected.take()
    } else {
        None
    };
    let height = (ui.available_height() * 0.4)
        .clamp(40.0, 220.0)
        .min(ui.available_height().max(0.0));
    egui::ScrollArea::vertical()
        .id_salt(("cloud-replies", session))
        .max_height(height)
        .show(ui, |ui| {
            for (index, r) in answers.iter().enumerate() {
                let reveal = selected_id.as_deref() == Some(r.id.as_str());
                let response = egui::CollapsingHeader::new(format!(
                    "{} · {}",
                    catalog.t("cloud.history_answer", &[]),
                    crate::ui::notifications::relative_time_label(
                        catalog,
                        deppy_core::time::unix_secs_i64() - r.created
                    )
                ))
                .id_salt(("cloud-answer", &r.id))
                .default_open(index == 0)
                .open(reveal.then_some(true))
                .show(ui, |ui| {
                    ui.add(egui::Label::new(&r.message).wrap().selectable(true));
                    if ui.button(catalog.t("cloud.copy_answer", &[])).clicked() {
                        ui.ctx().copy_text(r.message.clone());
                    }
                });
                if reveal {
                    response
                        .header_response
                        .scroll_to_me(Some(egui::Align::TOP));
                }
            }
        });
    ui.separator();
}
#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable as _;
    #[test]
    fn answers_display_only_in_the_original_session_and_copy_without_input() {
        let catalog = i18n::Catalog::load("ko-KR").unwrap();
        let records = vec![Answer {
            id: "answer".into(),
            session: "original".into(),
            created: deppy_core::time::unix_secs_i64(),
            message: "Grok final answer".into(),
        }];
        let mut harness = egui_kittest::Harness::builder().with_size(egui::vec2(700.0,500.0))
            .build_ui_state(move |ui,state:&mut (String,Option<String>,bool)| {
                ui.style_mut().animation_time=0.0;
                contents(ui,&records,&state.0,&mut state.1,&catalog);
                state.2 |= ui.ctx().output(|o|o.commands.iter().any(|c|matches!(c,egui::OutputCommand::CopyText(s) if s=="Grok final answer")));
            }, ("other".into(), None,false));
        assert!(harness.query_by_label("Grok final answer").is_none());
        harness.state_mut().0 = "original".into();
        harness.run();
        harness.get_by_label("Grok final answer");
        harness.get_by_label("답변 복사").click();
        harness.run();
        assert!(harness.state().2);
    }
}
