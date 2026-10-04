//! Document/path popup presentation. App owns queues, identity and I/O.
use super::popup::{self, ActionTone, NoticeTone, PopupSpec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirtyChoice {
    Save,
    Discard,
    Cancel,
}

pub(crate) struct DirtyDialog<'a> {
    pub id: egui::Id,
    pub target: egui::Id,
    pub name: &'a str,
    pub queued_after: usize,
    pub can_save: bool,
    pub save_too_large: bool,
}

// Actions are declared right to left, matching the shared footer and HTML.
fn decision<C: Copy>(
    ctx: &egui::Context,
    spec: PopupSpec<'_>,
    target: egui::Id,
    contents: impl FnOnce(&mut egui::Ui),
    actions: &[(C, String, ActionTone, bool)],
    dismiss: C,
) -> Option<C> {
    let id = spec.id;
    let close_enabled = spec.close_enabled;
    popup::prepare_target(ctx, id, target);
    let mut choice = None;
    let close = popup::show(ctx, spec, |ui| {
        popup::body(ui, contents);
        popup::footer(ui, None, |ui| {
            ui.push_id(target, |ui| {
                for (value, label, tone, enabled) in actions {
                    if popup::action_button(ui, label, *tone, *enabled).clicked() {
                        choice = Some(*value);
                    }
                }
            });
        });
    });
    if close || (close_enabled && popup::take_modal_escape(ctx, id)) {
        Some(dismiss)
    } else {
        choice
    }
}

pub(crate) fn dirty(
    ctx: &egui::Context,
    dialog: DirtyDialog<'_>,
    catalog: &i18n::Catalog,
) -> Option<DirtyChoice> {
    decision(
        ctx,
        PopupSpec {
            id: dialog.id,
            width: 480.0,
            title: &catalog.t("document.confirm_discard.title", &[]),
            subtitle: dialog.name,
            close_label: &catalog.t("popup.dismiss", &[]),
            close_enabled: true,
        },
        dialog.target,
        |ui| {
            popup::notice(
                ui,
                &catalog.t("document.confirm_discard.warning", &[]),
                NoticeTone::Error,
            );
            if dialog.queued_after > 0 {
                ui.label(catalog.t(
                    "document.confirm_discard.queued",
                    &[("count", &dialog.queued_after.to_string())],
                ));
            }
            if dialog.save_too_large {
                popup::notice(
                    ui,
                    &catalog.t("document.limit.save_too_large", &[]),
                    NoticeTone::Error,
                );
            }
        },
        &[
            (
                DirtyChoice::Discard,
                catalog.t("document.confirm_discard.discard", &[]),
                ActionTone::Danger,
                true,
            ),
            (
                DirtyChoice::Save,
                catalog.t("document.confirm_discard.save", &[]),
                ActionTone::Secondary,
                dialog.can_save,
            ),
            (
                DirtyChoice::Cancel,
                catalog.t("document.confirm_discard.cancel", &[]),
                ActionTone::Ghost,
                true,
            ),
        ],
        DirtyChoice::Cancel,
    )
}

pub(crate) fn conflict(
    ctx: &egui::Context,
    id: egui::Id,
    target: egui::Id,
    name: &str,
    queued_after: usize,
    catalog: &i18n::Catalog,
) -> Option<bool> {
    decision(
        ctx,
        PopupSpec {
            id,
            width: 400.0,
            title: &catalog.t("document.conflict.title", &[]),
            subtitle: name,
            close_label: &catalog.t("popup.dismiss", &[]),
            close_enabled: true,
        },
        target,
        |ui| {
            popup::notice(
                ui,
                &catalog.t("document.conflict.body", &[("name", name)]),
                NoticeTone::Error,
            );
            if queued_after > 0 {
                ui.label(catalog.t(
                    "document.conflict.queued",
                    &[("count", &queued_after.to_string())],
                ));
            }
        },
        &[
            (
                true,
                catalog.t("document.conflict.reload", &[]),
                ActionTone::Danger,
                true,
            ),
            (
                false,
                catalog.t("document.conflict.cancel", &[]),
                ActionTone::Ghost,
                true,
            ),
        ],
        false,
    )
}

pub(crate) fn cap(ctx: &egui::Context, catalog: &i18n::Catalog) -> bool {
    decision(
        ctx,
        PopupSpec {
            id: egui::Id::new("document_cap_notice"),
            width: 420.0,
            title: &catalog.t("document.cap.title", &[]),
            subtitle: "",
            close_label: &catalog.t("popup.dismiss", &[]),
            close_enabled: true,
        },
        egui::Id::new("document_cap_notice"),
        |ui| {
            popup::notice(ui, &catalog.t("document.cap.full", &[]), NoticeTone::Info);
        },
        &[(
            true,
            catalog.t("action.close", &[]),
            ActionTone::Primary,
            true,
        )],
        true,
    )
    .is_some()
}

pub(crate) struct MovedDialog<'a> {
    pub id: egui::Id,
    pub old: &'a str,
    pub new: &'a str,
    pub can_update: bool,
    pub submitting: bool,
    pub failed: bool,
}

pub(crate) fn moved(
    ctx: &egui::Context,
    dialog: MovedDialog<'_>,
    catalog: &i18n::Catalog,
) -> Option<bool> {
    decision(
        ctx,
        PopupSpec {
            id: dialog.id,
            width: 480.0,
            title: &catalog.t("workspace.folder_moved.title", &[]),
            subtitle: "",
            close_label: &catalog.t("popup.dismiss", &[]),
            close_enabled: !dialog.submitting,
        },
        egui::Id::new((dialog.old, dialog.new)),
        |ui| {
            popup::notice(
                ui,
                &catalog.t("workspace.folder_moved.body", &[]),
                NoticeTone::Info,
            );
            for (key, path) in [
                ("workspace.folder_moved.from", dialog.old),
                ("workspace.folder_moved.to", dialog.new),
            ] {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(catalog.t(key, &[("path", path)])).size(12.0),
                    )
                    .wrap(),
                );
            }
            if dialog.failed {
                popup::notice(
                    ui,
                    &catalog.t("workspace.folder_moved.failed", &[]),
                    NoticeTone::Error,
                );
            }
        },
        &[
            (
                true,
                catalog.t("workspace.folder_moved.update", &[]),
                ActionTone::Primary,
                dialog.can_update && !dialog.submitting,
            ),
            (
                false,
                catalog.t("workspace.folder_moved.ignore", &[]),
                ActionTone::Ghost,
                !dialog.submitting,
            ),
        ],
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    fn catalog() -> i18n::Catalog {
        i18n::Catalog::load("en-US").unwrap()
    }

    #[test]
    fn popup_review_next_dirty_document_does_not_inherit_discard_focus() {
        let catalog = catalog();
        let label = catalog.t("document.confirm_discard.discard", &[]);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (usize, Vec<DirtyChoice>)| {
                if state.0 < 2
                    && let Some(choice) = dirty(
                        ui.ctx(),
                        DirtyDialog {
                            target: egui::Id::new(state.0),
                            id: egui::Id::new("document_dirty_confirmation"),
                            name: "notes.md",
                            queued_after: 1 - state.0,
                            can_save: true,
                            save_too_large: false,
                        },
                        &catalog,
                    )
                {
                    state.1.push(choice);
                    state.0 += 1;
                }
            },
            (0, Vec::new()),
        );
        harness.run();
        harness.get_by_label(&label).focus();
        harness.run();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(harness.state().1, vec![DirtyChoice::Discard]);
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![DirtyChoice::Discard],
            "the second document needs a fresh explicit choice"
        );
    }

    #[test]
    fn popup_review_next_conflict_does_not_inherit_reload_focus() {
        let catalog = catalog();
        let label = catalog.t("document.conflict.reload", &[]);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (usize, Vec<bool>)| {
                if state.0 < 2
                    && let Some(choice) = conflict(
                        ui.ctx(),
                        egui::Id::new("document_conflict_confirmation"),
                        egui::Id::new(state.0),
                        "notes.md",
                        1 - state.0,
                        &catalog,
                    )
                {
                    state.1.push(choice);
                    state.0 += 1;
                }
            },
            (0, Vec::new()),
        );
        harness.run();
        harness.get_by_label(&label).focus();
        harness.run();
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(harness.state().1, vec![true]);
        harness.key_press(egui::Key::Enter);
        harness.run();
        assert_eq!(
            harness.state().1,
            vec![true],
            "the second conflict needs a fresh explicit choice"
        );
    }

    #[test]
    fn popup_review_long_translated_footer_fits_narrow_short_viewport() {
        for locale in ["en-US", "ko-KR", "ja-JP", "zh-Hans", "zh-Hant"] {
            let catalog = i18n::Catalog::load(locale).unwrap();
            let cancel = catalog.t("document.conflict.cancel", &[]);
            let reload = catalog.t("document.conflict.reload", &[]);
            let id = egui::Id::new("document_conflict_confirmation");
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(280.0, 360.0))
                .build_ui(move |ui| {
                    let _ = conflict(
                        ui.ctx(),
                        id,
                        egui::Id::new("notes"),
                        "a-long-project-document-name.md",
                        3,
                        &catalog,
                    );
                });
            crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
            harness.run();
            let cancel_rect = harness.get_by_label(&cancel).rect();
            assert!(
                (cancel_rect.height() - 34.0).abs() < 0.5,
                "{locale}: short cancel height {cancel_rect:?}"
            );
            let area = harness.ctx.memory(|m| m.area_rect(id)).unwrap();
            assert!(
                area.left() >= 0.0
                    && area.right() <= 280.0
                    && area.top() >= 0.0
                    && area.bottom() <= 360.0,
                "{locale}: {area:?}"
            );
            for label in [&cancel, &reload] {
                let rect = harness.get_by_label(label).rect();
                assert!(
                    rect.left() >= 0.0
                        && rect.right() <= 280.0
                        && rect.top() >= 0.0
                        && rect.bottom() <= 360.0,
                    "{locale}/{label}: {rect:?}"
                );
            }
        }
    }

    #[test]
    fn document_popups_dirty_buttons_keep_three_distinct_choices() {
        for (key, expected) in [
            ("document.confirm_discard.save", DirtyChoice::Save),
            ("document.confirm_discard.discard", DirtyChoice::Discard),
            ("document.confirm_discard.cancel", DirtyChoice::Cancel),
        ] {
            let catalog = catalog();
            let label = catalog.t(key, &[]);
            let mut harness = egui_kittest::Harness::new_ui_state(
                move |ui, choice| {
                    if choice.is_none() {
                        *choice = dirty(
                            ui.ctx(),
                            DirtyDialog {
                                target: egui::Id::new("fixture"),
                                id: egui::Id::new("dirty_test"),
                                name: "notes.md",
                                queued_after: 2,
                                can_save: true,
                                save_too_large: false,
                            },
                            &catalog,
                        );
                    }
                },
                None,
            );
            harness.run();
            assert!((harness.get_by_label(&label).rect().height() - 34.0).abs() < 0.5);
            harness.key_press(egui::Key::Enter);
            harness.run();
            assert_eq!(*harness.state(), None, "bare Enter must not discard");
            harness.get_by_label(&label).click();
            harness.run();
            assert_eq!(*harness.state(), Some(expected));
        }
    }

    #[test]
    fn document_popups_disabled_save_preserves_choice_and_shows_reason() {
        let catalog = catalog();
        let save = catalog.t("document.confirm_discard.save", &[]);
        let reason = catalog.t("document.limit.save_too_large", &[]);
        let queued = catalog.t("document.confirm_discard.queued", &[("count", "2")]);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, choice| {
                if choice.is_none() {
                    *choice = dirty(
                        ui.ctx(),
                        DirtyDialog {
                            target: egui::Id::new("fixture"),
                            id: egui::Id::new("dirty_limit"),
                            name: "large.md",
                            queued_after: 2,
                            can_save: false,
                            save_too_large: true,
                        },
                        &catalog,
                    );
                }
            },
            None,
        );
        harness.run();
        assert!(harness.query_by_label(&reason).is_some());
        assert!(harness.query_by_label(&queued).is_some());
        let pos = harness.get_by_label(&save).rect().center();
        harness.hover_at(pos);
        harness.run();
        harness.drag_at(pos);
        harness.run();
        harness.drop_at(pos);
        harness.run();
        assert_eq!(*harness.state(), None);
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert_eq!(*harness.state(), Some(DirtyChoice::Cancel));
    }

    #[test]
    fn document_popups_dirty_x_and_backdrop_cancel() {
        for backdrop in [false, true] {
            let catalog = catalog();
            let close = catalog.t("popup.dismiss", &[]);
            let mut harness = egui_kittest::Harness::new_ui_state(
                move |ui, choice| {
                    if choice.is_none() {
                        *choice = dirty(
                            ui.ctx(),
                            DirtyDialog {
                                target: egui::Id::new("fixture"),
                                id: egui::Id::new("dirty_dismiss"),
                                name: "notes.md",
                                queued_after: 0,
                                can_save: true,
                                save_too_large: false,
                            },
                            &catalog,
                        );
                    }
                },
                None,
            );
            harness.run();
            if backdrop {
                let pos = egui::pos2(3.0, 3.0);
                harness.hover_at(pos);
                harness.run();
                harness.drag_at(pos);
                harness.run();
                harness.drop_at(pos);
            } else {
                harness.get_by_label(&close).click();
            }
            harness.run();
            assert_eq!(*harness.state(), Some(DirtyChoice::Cancel));
        }
    }

    #[test]
    fn document_popups_conflict_reload_cancel_and_escape() {
        for action in [Some(true), Some(false), None] {
            let catalog = catalog();
            let label = catalog.t(
                if action == Some(true) {
                    "document.conflict.reload"
                } else {
                    "document.conflict.cancel"
                },
                &[],
            );
            let mut harness = egui_kittest::Harness::new_ui_state(
                move |ui, choice| {
                    if choice.is_none() {
                        *choice = conflict(
                            ui.ctx(),
                            egui::Id::new("conflict_test"),
                            egui::Id::new("notes"),
                            "notes.md",
                            1,
                            &catalog,
                        );
                    }
                },
                None,
            );
            harness.run();
            harness.key_press(egui::Key::Enter);
            harness.run();
            assert_eq!(*harness.state(), None);
            if action.is_some() {
                harness.get_by_label(&label).click();
            } else {
                harness.key_press(egui::Key::Escape);
            }
            harness.run();
            assert_eq!(*harness.state(), Some(action.unwrap_or(false)));
        }
    }

    #[test]
    fn document_popups_cap_close_and_escape_acknowledge() {
        for escape in [false, true] {
            let catalog = catalog();
            let label = catalog.t("action.close", &[]);
            let mut harness = egui_kittest::Harness::new_ui_state(
                move |ui, acknowledged| {
                    if !*acknowledged {
                        *acknowledged = cap(ui.ctx(), &catalog);
                    }
                },
                false,
            );
            harness.run();
            if escape {
                harness.key_press(egui::Key::Escape);
            } else {
                harness.get_by_label(&label).click();
            }
            harness.run();
            assert!(*harness.state());
        }
    }

    #[test]
    fn document_popups_moved_busy_submission_and_retry_keep_paths() {
        let catalog = catalog();
        let update = catalog.t("workspace.folder_moved.update", &[]);
        let from = catalog.t("workspace.folder_moved.from", &[("path", "/old/Serenity")]);
        let to = catalog.t("workspace.folder_moved.to", &[("path", "/new/Serenity")]);
        let error = catalog.t("workspace.folder_moved.failed", &[]);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, state: &mut (Option<bool>, bool, bool)| {
                if state.0.is_none() {
                    state.0 = moved(
                        ui.ctx(),
                        MovedDialog {
                            id: egui::Id::new("moved_test"),
                            old: "/old/Serenity",
                            new: "/new/Serenity",
                            can_update: state.1,
                            submitting: state.2,
                            failed: true,
                        },
                        &catalog,
                    );
                }
            },
            (None, false, false),
        );
        harness.run();
        assert!(harness.query_by_label(&from).is_some() && harness.query_by_label(&to).is_some());
        assert!(harness.query_by_label(&error).is_some());
        let pos = harness.get_by_label(&update).rect().center();
        harness.hover_at(pos);
        harness.run();
        harness.drag_at(pos);
        harness.run();
        harness.drop_at(pos);
        harness.run();
        assert_eq!(harness.state().0, None);
        harness.state_mut().2 = true;
        harness.run();
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert_eq!(
            harness.state().0,
            None,
            "accepted update cannot be dismissed while submitting"
        );
        harness.state_mut().1 = true;
        harness.state_mut().2 = false;
        harness.run();
        harness.get_by_label(&update).click();
        harness.run();
        assert_eq!(harness.state().0, Some(true));
    }

    #[test]
    fn document_popups_moved_ignore_does_not_update() {
        let catalog = catalog();
        let label = catalog.t("workspace.folder_moved.ignore", &[]);
        let mut harness = egui_kittest::Harness::new_ui_state(
            move |ui, choice| {
                if choice.is_none() {
                    *choice = moved(
                        ui.ctx(),
                        MovedDialog {
                            id: egui::Id::new("moved_ignore"),
                            old: "/old",
                            new: "/new",
                            can_update: true,
                            submitting: false,
                            failed: false,
                        },
                        &catalog,
                    );
                }
            },
            None,
        );
        harness.run();
        harness.get_by_label(&label).click();
        harness.run();
        assert_eq!(*harness.state(), Some(false));
    }

    #[test]
    #[ignore = "offscreen PNGs for manual visual review"]
    fn popup_parity_render_document_popups() {
        for (case, locale, size, suffix) in [
            (26, "ko-KR", egui::vec2(800.0, 650.0), ""),
            (27, "ko-KR", egui::vec2(800.0, 650.0), ""),
            (28, "ko-KR", egui::vec2(800.0, 650.0), ""),
            (29, "ko-KR", egui::vec2(800.0, 650.0), ""),
            (27, "en-US", egui::vec2(280.0, 360.0), "-narrow"),
        ] {
            let catalog = i18n::Catalog::load(locale).unwrap();
            let mut harness =
                egui_kittest::Harness::builder()
                    .with_size(size)
                    .build_ui(move |ui| match case {
                        26 => {
                            dirty(
                                ui.ctx(),
                                DirtyDialog {
                                    target: egui::Id::new("fixture"),
                                    id: egui::Id::new(case),
                                    name: "notes.md",
                                    queued_after: 2,
                                    can_save: true,
                                    save_too_large: false,
                                },
                                &catalog,
                            );
                        }
                        27 => {
                            conflict(
                                ui.ctx(),
                                egui::Id::new(case),
                                egui::Id::new("notes"),
                                "notes.md",
                                1,
                                &catalog,
                            );
                        }
                        28 => {
                            cap(ui.ctx(), &catalog);
                        }
                        _ => {
                            moved(
                                ui.ctx(),
                                MovedDialog {
                                    id: egui::Id::new(case),
                                    old: "/Users/jr/Projects/Serenity",
                                    new: "/Users/jr/Archive/Serenity",
                                    can_update: true,
                                    submitting: false,
                                    failed: false,
                                },
                                &catalog,
                            );
                        }
                    });
            crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
            harness.ctx.set_visuals(egui::Visuals::dark());
            harness.run();
            let output = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/popup-parity");
            std::fs::create_dir_all(&output).unwrap();
            harness
                .render()
                .unwrap()
                .save(output.join(format!("{case}-document{suffix}.png")))
                .unwrap();
        }
    }

    #[test]
    fn document_popups_long_paths_and_warning_fit_narrow_screen() {
        for case in [26, 29] {
            let catalog = i18n::Catalog::load("ko-KR").unwrap();
            let id = egui::Id::new(("narrow_document", case));
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(280.0, 360.0)).build_ui(move |ui| {
                    if case == 26 {
                        dirty(ui.ctx(), DirtyDialog { target: egui::Id::new("fixture"), id, name: "long-long-long-document-name.md", queued_after: 5,
                            can_save: false, save_too_large: true }, &catalog);
                    } else {
                        moved(ui.ctx(), MovedDialog { id,
                            old: "/Users/jr/Projects/a-very-long-project-name/very-long-folder-name/Serenity",
                            new: "/Users/jr/Archive/a-very-long-project-name/very-long-folder-name/Serenity",
                            can_update: true, submitting: false, failed: true }, &catalog);
                    }
                });
            crate::fonts::install_cjk_fallback(&harness.ctx, None, "JetBrainsMono", "Regular");
            harness.run();
            let rect = harness.ctx.memory(|memory| memory.area_rect(id)).unwrap();
            assert!(
                rect.left() >= 0.0
                    && rect.right() <= 280.0
                    && rect.top() >= 0.0
                    && rect.bottom() <= 360.0,
                "{case}: {rect:?}"
            );
        }
    }
}
