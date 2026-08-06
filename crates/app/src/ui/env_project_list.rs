#[derive(Debug, Clone)]
pub struct EnvProjectRow {
    pub id: String,
    pub name: String,
    /// 사용자 별칭(name 컬럼 원본, E3) — 표시명은 `폴더명 (별칭)`으로 파생되므로
    /// 이름 편집 폼은 표시명이 아니라 이 값을 초기값으로 쓴다.
    pub alias: String,
    pub path: String,
    /// 저장 경로가 디스크에 없음(폴더 이동/삭제, EXDEV 볼륨 이동, 셸 부재로 자동 복구
    /// 불가) — 경로를 경고색으로 표시해 재선택을 유도한다(2026-07-09).
    pub path_missing: bool,
    pub env_count: usize,
    pub key_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvProjectListAction {
    None,
    Select(String),
    AddRequested,
    CloseRequested(String),
}

#[derive(Debug, Clone)]
pub struct EnvProjectListStyle {
    pub width: f32,
    pub header_height: f32,
    pub row_height: f32,
    pub padding_x: f32,
    pub count_width: f32,
    pub delete_width: f32,
    pub gap: f32,
    pub add_button_size: f32,
    pub delete_button_height: f32,
    pub name_font_size: f32,
    pub path_font_size: f32,
    pub count_font_size: f32,
}

impl Default for EnvProjectListStyle {
    fn default() -> Self {
        Self {
            width: 220.0,
            // 참조 목업(ProjectList.tsx)의 CSS logical px를 그대로 사용한다.
            header_height: 42.0,
            row_height: 76.0,
            padding_x: 10.0,
            count_width: 52.0,
            delete_width: 22.0,
            gap: 6.0,
            add_button_size: 18.0,
            delete_button_height: 20.0,
            name_font_size: 14.0,
            path_font_size: 12.0,
            count_font_size: 12.0,
        }
    }
}

impl EnvProjectListStyle {
    pub fn for_available_width(available_width: f32) -> Self {
        let mut style = Self::default();
        style.width = if available_width.is_finite() {
            (available_width * 0.34).clamp(176.0, style.width)
        } else {
            style.width
        };
        style
    }
}

// 이 화면(환경 설정의 프로젝트 rail)은 설정 창 안에서 렌더되므로 settings.rs 팔레트와
// 같은 축(hsl 220도)을 써야 한다. 2026-08-06 팔레트 통일 전 값들이 사본으로 남아 있어
// 설정 창의 다른 면이 축 위로 옮겨간 뒤 이 rail만 무채색으로 튀었다.

pub fn panel_bg(ui: &egui::Ui) -> egui::Color32 {
    // settings의 '관리 panel'과 값이 달라(#1e1e1e vs #202020) 헬퍼로 위임하지 않고,
    // 이 화면 고유의 명도는 유지한 채 색상축만 맞춘다.
    if ui.visuals().dark_mode {
        egui::Color32::from_rgb(0x18, 0x1b, 0x20) // was #1e1e1e (무채색)
    } else {
        egui::Color32::from_rgb(0xf9, 0xfa, 0xfb) // was #fafafa (무채색)
    }
}

fn text_secondary(ui: &egui::Ui) -> egui::Color32 {
    // #aaaaaa/#444444는 settings 보조색의 통일 이전 값 그대로였다 — 헬퍼로 위임한다.
    super::settings::settings_text_secondary(ui)
}

/// navActive 토큰 — egui visuals에 대응 색이 없어 settings.rs 헬퍼를 재사용한다.
/// (#2e4a5e/#ccdeed는 그 헬퍼의 통일 이전 값이 복사돼 있던 것이다.)
fn tok_nav_active(ui: &egui::Ui) -> egui::Color32 {
    super::settings::settings_nav_active(ui)
}

pub fn render_with_style(
    ui: &mut egui::Ui,
    projects: &[EnvProjectRow],
    active_id: &str,
    catalog: &i18n::Catalog,
    style: &EnvProjectListStyle,
) -> EnvProjectListAction {
    let bg = panel_bg(ui);
    let panel_height = ui.available_height().max(style.header_height);
    let (panel_rect, _) =
        ui.allocate_exact_size(egui::vec2(style.width, panel_height), egui::Sense::hover());
    ui.painter().rect_filled(panel_rect, 0.0, bg);

    let header_rect =
        egui::Rect::from_min_size(panel_rect.min, egui::vec2(style.width, style.header_height));
    let add_requested = paint_header(ui, header_rect, style, catalog);

    let divider_color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    super::hairline_at(
        ui.painter(),
        header_rect.x_range(),
        header_rect.bottom(),
        divider_color,
    );

    let list_rect = egui::Rect::from_min_max(header_rect.left_bottom(), panel_rect.right_bottom());

    let mut action = if add_requested {
        EnvProjectListAction::AddRequested
    } else {
        EnvProjectListAction::None
    };

    if projects.is_empty() {
        // 사용자가 마지막 항목까지 닫을 수 있다. 상단 `+`만 유지하고 목록 본문은
        // 별도 empty-state 문구 없이 비워 둔다.
        return action;
    }

    let mut list_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(list_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    list_ui.set_width(style.width);

    // 행/divider/선택 배경 폭은 전부 panel_rect(=패널 배경·헤더와 동일) 기준으로
    // 통일한다. ui.available_width()는 ScrollArea 유무/레이아웃에 따라 달라져
    // 우측 경계선 침범 또는 빈 여백 띠의 원인이 됐다(2026-07-10).
    let row_width = list_rect.width();
    let content_height = projects.len() as f32 * style.row_height;
    if content_height <= list_rect.height() {
        render_rows(
            &mut list_ui,
            projects,
            active_id,
            row_width,
            style,
            catalog,
            &mut action,
        );
    } else {
        egui::ScrollArea::vertical()
            .id_salt("env_project_list_scroll")
            .auto_shrink([false, false])
            .show(&mut list_ui, |ui| {
                render_rows(
                    ui,
                    projects,
                    active_id,
                    row_width,
                    style,
                    catalog,
                    &mut action,
                );
            });
    }

    action
}

fn render_rows(
    ui: &mut egui::Ui,
    projects: &[EnvProjectRow],
    active_id: &str,
    row_width: f32,
    style: &EnvProjectListStyle,
    catalog: &i18n::Catalog,
    action: &mut EnvProjectListAction,
) {
    ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
    for project in projects {
        let selected = project.id == active_id;
        let next = render_row(ui, project, selected, row_width, style, catalog);
        if !matches!(next, EnvProjectListAction::None) {
            *action = next;
        }
    }
}

fn paint_header(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    style: &EnvProjectListStyle,
    catalog: &i18n::Catalog,
) -> bool {
    // ProjectList.tsx: 13px muted label, 10px horizontal inset.
    ui.painter().text(
        egui::pos2(rect.left() + style.padding_x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        catalog.t("env.projects", &[]),
        egui::FontId::monospace(13.0),
        ui.visuals().weak_text_color(),
    );

    let add_rect = egui::Rect::from_center_size(
        egui::pos2(
            rect.right() - style.padding_x - style.add_button_size / 2.0,
            rect.center().y,
        ),
        egui::vec2(style.add_button_size, style.add_button_size),
    );
    let add = ui
        .interact(
            add_rect,
            ui.id().with("env_project_add"),
            egui::Sense::click(),
        )
        .on_hover_text(catalog.t("workspace.manager.new_hint", &[]));
    let add_fill = if add.hovered() {
        ui.visuals().selection.bg_fill
    } else {
        ui.visuals().extreme_bg_color
    };
    ui.painter().rect_filled(add_rect, 0.0, add_fill);
    ui.painter().rect_stroke(
        add_rect,
        0.0,
        if add.hovered() {
            egui::Stroke::new(1.0, ui.visuals().selection.bg_fill)
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke
        },
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        add_rect.center(),
        egui::Align2::CENTER_CENTER,
        "+",
        egui::FontId::monospace(16.0),
        if add.hovered() {
            egui::Color32::WHITE
        } else {
            ui.visuals().weak_text_color()
        },
    );
    add.clicked()
}

fn render_row(
    ui: &mut egui::Ui,
    project: &EnvProjectRow,
    selected: bool,
    row_width: f32,
    style: &EnvProjectListStyle,
    catalog: &i18n::Catalog,
) -> EnvProjectListAction {
    let bg = panel_bg(ui);
    // 행 폭은 render_with_style이 잰 panel_rect 폭(row_width) 고정 — 패널 배경·
    // 헤더·divider와 우측 끝이 항상 일치한다. 스크롤바는 floating(예약 폭 0)이라
    // ScrollArea 유무와 무관하게 같은 폭이 유지된다(2026-07-10).
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(row_width, style.row_height),
        egui::Sense::click(),
    );
    let fill = if selected {
        tok_nav_active(ui)
    } else if response.hovered() {
        ui.visuals().widgets.hovered.weak_bg_fill
    } else {
        bg
    };

    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, fill);
    // CSS border-bottom: 1px. 별도 레이아웃 행을 소비하지 않아 총 행 높이가 76px다.
    // 다음 행 fill이 이전 border를 덮어도 현재 top에서 같은 1px을 복원한다.
    super::hairline_at(
        painter,
        rect.x_range(),
        rect.top(),
        ui.visuals().widgets.noninteractive.bg_stroke.color,
    );
    super::hairline_at(
        painter,
        rect.x_range(),
        rect.bottom(),
        ui.visuals().widgets.noninteractive.bg_stroke.color,
    );

    // 삭제 버튼은 이름/env count가 있는 위쪽 줄과 세로 중심을 맞춘다.
    let name_y = rect.top() + 23.0;
    let path_y = rect.top() + 50.0;
    let delete_top = name_y - style.delete_button_height / 2.0;
    let delete_rect = egui::Rect::from_min_size(
        egui::pos2(
            rect.right() - style.padding_x - style.delete_width,
            delete_top,
        ),
        egui::vec2(style.delete_width, style.delete_button_height),
    );
    let count_right = delete_rect.left() - style.gap;
    let count_left = count_right - style.count_width;
    let text_right = count_left - style.gap;

    let name_clip = egui::Rect::from_min_max(
        egui::pos2(rect.left() + style.padding_x, rect.top()),
        egui::pos2(text_right, rect.center().y),
    );
    let name_pos = egui::pos2(rect.left() + style.padding_x, name_y);
    let name_font = egui::FontId::monospace(style.name_font_size);
    let name_color = if selected {
        ui.visuals().text_color()
    } else {
        text_secondary(ui)
    };
    // fonts.rs가 Regular 페이스만 등록해 Bold 지정이 불가 — 겹쳐그리기(faux-bold)는
    // 흐림을 유발하므로 쓰지 않고 한 번만 그린다(사용자 2026-07-09). 진짜 Bold가
    // 필요하면 fonts.rs에 Bold 페이스 등록이 선행돼야 한다.
    painter.with_clip_rect(name_clip).text(
        name_pos,
        egui::Align2::LEFT_CENTER,
        &project.name,
        name_font,
        name_color,
    );

    let path = display_project_path(&project.path);
    let path_clip = egui::Rect::from_min_max(
        egui::pos2(rect.left() + style.padding_x, rect.center().y),
        egui::pos2(text_right, rect.bottom()),
    );
    // 경로가 사라진 프로젝트(rename 자동 복구 불가 — EXDEV/셸 부재)는 경고색으로.
    let path_color = if project.path_missing {
        ui.visuals().error_fg_color
    } else if selected {
        text_secondary(ui)
    } else {
        ui.visuals().weak_text_color()
    };
    painter.with_clip_rect(path_clip).text(
        egui::pos2(rect.left() + style.padding_x, path_y),
        egui::Align2::LEFT_CENTER,
        path,
        egui::FontId::monospace(style.path_font_size),
        path_color,
    );
    if project.path_missing {
        // 경로 영역 hover에 원인/조치 안내.
        let hint = ui.interact(
            path_clip,
            ui.id().with(("env_project_path_missing", &project.id)),
            egui::Sense::hover(),
        );
        hint.on_hover_text(catalog.t("env.project_path_missing", &[]));
    }

    painter.text(
        egui::pos2(count_right, name_y),
        egui::Align2::RIGHT_CENTER,
        format!("{}env", project.env_count),
        egui::FontId::monospace(style.count_font_size),
        if selected {
            text_secondary(ui)
        } else {
            ui.visuals().weak_text_color()
        },
    );
    painter.text(
        egui::pos2(count_right, path_y),
        egui::Align2::RIGHT_CENTER,
        format!("{}key", project.key_count),
        egui::FontId::monospace(style.count_font_size),
        if selected {
            text_secondary(ui)
        } else {
            ui.visuals().weak_text_color()
        },
    );

    // 선택 행은 항상, 나머지는 hover 때만 ×를 노출한다. 이 버튼은 Environment & API
    // 목록만 닫으며 workspace 자체를 삭제하거나 sidebar runtime을 종료하지 않는다.
    if selected || response.hovered() {
        let close = ui
            .interact(
                delete_rect,
                ui.id().with(("env_project_close", &project.id)),
                egui::Sense::click(),
            )
            .on_hover_text(catalog.t("action.close", &[]));
        paint_close_button(ui, delete_rect, close.hovered());
        if close.clicked() {
            return EnvProjectListAction::CloseRequested(project.id.clone());
        }
    }

    if response.clicked() && !selected {
        EnvProjectListAction::Select(project.id.clone())
    } else {
        EnvProjectListAction::None
    }
}

fn paint_close_button(ui: &mut egui::Ui, rect: egui::Rect, hovered: bool) {
    let fill = if hovered {
        ui.visuals().widgets.hovered.weak_bg_fill
    } else {
        egui::Color32::TRANSPARENT
    };
    let stroke = if hovered {
        ui.visuals().widgets.hovered.bg_stroke.color
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke.color
    };
    let text = if hovered {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    ui.painter().rect_filled(rect, 0.0, fill);
    ui.painter().rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::monospace(12.0),
        text,
    );
}

pub fn display_project_path(path: &str) -> String {
    if path.trim().is_empty() {
        return "~".to_owned();
    }
    if let Some(home) = crate::paths::home_dir() {
        let path_buf = std::path::PathBuf::from(path);
        if let Ok(stripped) = path_buf.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.to_owned()
}

#[cfg(test)]
mod tests {
    use super::EnvProjectListStyle;

    #[test]
    fn reference_project_list는_220_42_76_grid를_쓴다() {
        let style = EnvProjectListStyle::default();
        assert_eq!(style.width, 220.0);
        assert_eq!(style.header_height, 42.0);
        assert_eq!(style.row_height, 76.0);
        assert_eq!(style.padding_x, 10.0);
    }
}
