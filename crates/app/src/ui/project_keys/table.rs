use deppy_core::project_keys::*;
use egui::{Align2, FontId, Rect, Sense, Ui, vec2};

pub(super) const ROW_HEIGHT: f32 = 44.0;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RowAction {
    Select,
    Edit,
    Copy,
}

pub(super) fn source_label(source: &KeySource, catalog: &i18n::Catalog) -> String {
    match source {
        KeySource::Vault => catalog.t("project_keys.source_vault", &[]),
        KeySource::Dotenv { file } => file.clone(),
        KeySource::PemFile { path } => path.clone(),
        KeySource::GitHubOnly => catalog.t("project_keys.source_github", &[]),
    }
}

pub(super) fn usage_label(usage: KeyUsage, catalog: &i18n::Catalog) -> String {
    catalog.t(
        match usage {
            KeyUsage::Build => "project_keys.usage_build",
            KeyUsage::StoreOnly => "project_keys.usage_store",
            KeyUsage::LegacyAgent => "project_keys.usage_legacy",
        },
        &[],
    )
}

pub(super) fn storage_label(state: KeyStorageState, catalog: &i18n::Catalog) -> String {
    catalog.t(
        match state {
            KeyStorageState::NotStored => "project_keys.state_not_stored",
            KeyStorageState::Stored => "project_keys.state_stored",
            KeyStorageState::UpdateNeeded => "project_keys.state_update",
            KeyStorageState::Locked => "project_keys.state_locked",
            KeyStorageState::MissingSource => "project_keys.state_missing",
            KeyStorageState::EmptySource => "project_keys.state_empty",
            KeyStorageState::Conflict => "project_keys.state_conflict",
            KeyStorageState::Failed => "project_keys.state_failed",
        },
        &[],
    )
}

pub(super) fn remote_label(state: GitHubRemoteState, catalog: &i18n::Catalog) -> String {
    catalog.t(
        match state {
            GitHubRemoteState::Unlinked => "project_keys.remote_unlinked",
            GitHubRemoteState::Pending => "project_keys.remote_pending",
            GitHubRemoteState::Sent => "project_keys.remote_sent",
            GitHubRemoteState::Failed => "project_keys.remote_failed",
            GitHubRemoteState::ExternalChange => "project_keys.remote_external",
        },
        &[],
    )
}

pub(super) fn saved_time(seconds: i64) -> String {
    if seconds <= 0 {
        return "—".into();
    }
    let day = seconds.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02} UTC",
        deppy_core::time::civil_date(seconds),
        day / 3600,
        day / 60 % 60
    )
}

pub(super) fn change_label(kind: ProjectKeyChangeKind, catalog: &i18n::Catalog) -> String {
    catalog.t(
        match kind {
            ProjectKeyChangeKind::Created => "project_keys.change_created",
            ProjectKeyChangeKind::Updated => "project_keys.change_updated",
            ProjectKeyChangeKind::ValueSaved => "project_keys.change_value",
            ProjectKeyChangeKind::StorageChanged => "project_keys.change_storage",
            ProjectKeyChangeKind::RemoteChanged => "project_keys.change_remote",
        },
        &[],
    )
}

#[cfg(test)]
pub(super) fn row(
    ui: &mut Ui,
    key: &ProjectKey,
    selected: bool,
    enabled: bool,
    catalog: &i18n::Catalog,
) -> (Rect, Option<RowAction>) {
    row_with_name(ui, key, &key.name, selected, enabled, catalog)
}

pub(super) fn row_with_name(
    ui: &mut Ui,
    key: &ProjectKey,
    display_name: &str,
    selected: bool,
    enabled: bool,
    catalog: &i18n::Catalog,
) -> (Rect, Option<RowAction>) {
    let enabled = enabled && ui.is_enabled();
    ui.push_id(&key.id, |ui| {
        if !enabled {
            ui.disable();
        }
        row_contents(ui, key, display_name, selected, enabled, catalog)
    })
    .inner
}

fn row_contents(
    ui: &mut Ui,
    key: &ProjectKey,
    display_name: &str,
    selected: bool,
    enabled: bool,
    catalog: &i18n::Catalog,
) -> (Rect, Option<RowAction>) {
    let colors = super::super::popup::palette(ui);
    let copy_enabled = enabled && key.source != KeySource::GitHubOnly;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::hover());
    if selected {
        let accent = ui.visuals().selection.bg_fill;
        ui.painter().rect_filled(
            rect,
            0,
            egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 28),
        );
        ui.painter()
            .vline(rect.left(), rect.y_range(), egui::Stroke::new(2.0, accent));
    }
    ui.painter().hline(
        rect.x_range(),
        rect.bottom(),
        egui::Stroke::new(1.0, colors.line),
    );
    let narrow = rect.width() < 620.0;
    let actions_width = 114.0;
    let name_width = if narrow {
        (rect.width() - actions_width - 88.0).max(60.0)
    } else {
        rect.width() * 0.26
    };
    let name_rect = Rect::from_min_size(rect.min + vec2(6.0, 0.0), vec2(name_width, 26.0));
    let name = ui.put(
        name_rect,
        egui::Button::new("")
            .left_text(
                egui::RichText::new(display_name)
                    .size(13.0)
                    .color(colors.text),
            )
            .frame(false)
            .truncate(),
    );
    name.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, display_name));
    let origin = source_label(&key.source, catalog);
    let origin_rect = Rect::from_min_size(rect.min + vec2(10.0, 25.0), vec2(name_width, 17.0));
    ui.painter().with_clip_rect(origin_rect).text(
        origin_rect.left_center(),
        Align2::LEFT_CENTER,
        &origin,
        FontId::proportional(10.0),
        colors.muted,
    );
    ui.interact(origin_rect, ui.id().with("origin"), Sense::hover())
        .on_hover_text(&origin)
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, enabled, &origin));
    let copy_rect = Rect::from_min_size(
        egui::pos2(rect.right() - 54.0, rect.top() + 8.0),
        vec2(50.0, 28.0),
    );
    let edit_rect = copy_rect.translate(vec2(-56.0, 0.0));
    let edit = ui.put(
        edit_rect,
        egui::Button::new(catalog.t("project_keys.edit", &[])).corner_radius(3),
    );
    let copy = ui
        .scope(|ui| {
            if !copy_enabled {
                ui.disable();
            }
            ui.put(
                copy_rect,
                egui::Button::new(catalog.t("action.copy", &[])).corner_radius(3),
            )
        })
        .inner;
    edit.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            enabled,
            catalog.t("project_keys.edit_key", &[("name", display_name)]),
        )
    });
    copy.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            copy_enabled,
            catalog.t("project_keys.copy_key", &[("name", display_name)]),
        )
    });
    let value_left = rect.left() + name_width + 14.0;
    let value_width = if narrow {
        (edit_rect.left() - value_left - 5.0).max(24.0)
    } else {
        96.0
    };
    let value_rect = Rect::from_min_size(
        egui::pos2(value_left, rect.top() + 8.0),
        vec2(value_width, 28.0),
    );
    let value = ui
        .scope(|ui| {
            if !copy_enabled {
                ui.disable();
            }
            ui.put(
                value_rect,
                egui::Button::new(
                    egui::RichText::new(if key.source == KeySource::GitHubOnly {
                        "—"
                    } else if narrow {
                        "••"
                    } else {
                        "••••••"
                    })
                    .font(FontId::monospace(11.0)),
                )
                .fill(colors.input)
                .stroke(egui::Stroke::new(1.0, colors.border))
                .corner_radius(3)
                .truncate(),
            )
        })
        .inner;
    value.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            copy_enabled,
            catalog.t("project_keys.value_copy", &[("name", display_name)]),
        )
    });
    let value = value.on_hover_text(catalog.t("project_keys.double_copy", &[]));
    if !narrow {
        let start = value_rect.right() + 8.0;
        let available = (edit_rect.left() - start - 10.0).max(0.0);
        let usage = usage_label(key.usage, catalog);
        let state = storage_label(key.storage_state, catalog);
        let saved = key
            .last_good_revision
            .map(|v| {
                format!(
                    "v{v} · {}",
                    key.last_good_at
                        .map(saved_time)
                        .unwrap_or_else(|| "—".into())
                )
            })
            .unwrap_or_else(|| "—".into());
        let font = FontId::proportional(11.0);
        for (i, label) in [usage, state, saved].iter().enumerate() {
            let cell = Rect::from_min_size(
                egui::pos2(start + available * i as f32 / 3.0, rect.top()),
                vec2(available / 3.0, ROW_HEIGHT),
            );
            let painter = ui.painter().with_clip_rect(cell);
            painter.text(
                cell.left_center(),
                Align2::LEFT_CENTER,
                label,
                font.clone(),
                colors.muted,
            );
            ui.interact(cell, ui.id().with((&key.id, i)), Sense::hover())
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, enabled, label));
        }
    }
    let action = if !enabled {
        None
    } else if copy_enabled && (value.double_clicked() || copy.clicked()) {
        Some(RowAction::Copy)
    } else if edit.clicked() {
        Some(RowAction::Edit)
    } else if name.clicked() {
        Some(RowAction::Select)
    } else {
        None
    };
    (rect, action)
}

pub(super) fn header(ui: &mut Ui, catalog: &i18n::Catalog) {
    if ui.available_width() < 620.0 {
        return;
    }
    let colors = super::super::popup::palette(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
    let name_width = rect.width() * 0.26;
    let value_left = rect.left() + name_width + 14.0;
    let start = value_left + 104.0;
    let available = (rect.right() - 110.0 - start - 10.0).max(0.0);
    for (left, width, key) in [
        (rect.left() + 10.0, name_width, "project_keys.name_source"),
        (value_left, 96.0, "project_keys.value_column"),
        (start, available / 3.0, "project_keys.usage"),
        (
            start + available / 3.0,
            available / 3.0,
            "project_keys.state_column",
        ),
        (
            start + available * 2.0 / 3.0,
            available / 3.0,
            "project_keys.saved_column",
        ),
    ] {
        let cell = Rect::from_min_size(egui::pos2(left, rect.top()), vec2(width, 24.0));
        let label = catalog.t(key, &[]);
        ui.painter().with_clip_rect(cell).text(
            cell.left_center(),
            Align2::LEFT_CENTER,
            &label,
            FontId::proportional(11.0),
            colors.muted,
        );
        ui.interact(cell, ui.id().with(key), Sense::hover())
            .widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), &label)
            });
    }
}
