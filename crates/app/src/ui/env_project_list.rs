#[derive(Debug, Clone)]
pub struct EnvProjectRow {
    pub id: String,
    pub name: String,
    pub path: String,
    pub env_count: usize,
    pub key_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvProjectListAction {
    None,
    Select(String),
    AddRequested,
    DeleteRequested(String),
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
    pub row_text_offset_y: f32,
}

impl Default for EnvProjectListStyle {
    fn default() -> Self {
        Self {
            width: 220.0,
            header_height: 34.0,
            row_height: 58.0, // 세로 간격 축소(사용자 2026-07-09)
            padding_x: 10.0,
            count_width: 46.0,
            delete_width: 18.0,
            gap: 14.0, // 이름 clip과 env/key 카운트 사이 여유 — 닿아 보임 방지(2026-07-09)
            add_button_size: 16.0,
            delete_button_height: 18.0,
            name_font_size: 14.0,
            path_font_size: 12.0,
            count_font_size: 12.0,
            row_text_offset_y: 11.0,
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

pub fn panel_bg(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_rgb(0x20, 0x20, 0x20)
    } else {
        ui.visuals().faint_bg_color
    }
}

/// navActive 토큰 — egui visuals에 대응 색이 없어 로컬 상수로 정의한다.
fn tok_nav_active(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_rgb(0x2e, 0x4a, 0x5e)
    } else {
        egui::Color32::from_rgb(0xcc, 0xde, 0xed)
    }
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
    paint_hline(
        ui.painter(),
        header_rect.x_range().into(),
        header_rect.bottom(),
        divider_color,
    );

    let list_rect = egui::Rect::from_min_max(
        egui::pos2(panel_rect.left(), header_rect.bottom() + 1.0),
        panel_rect.right_bottom(),
    );

    let mut action = if add_requested {
        EnvProjectListAction::AddRequested
    } else {
        EnvProjectListAction::None
    };

    if projects.is_empty() {
        ui.painter().text(
            list_rect.center(),
            egui::Align2::CENTER_CENTER,
            catalog.t("env.projects_empty", &[]),
            egui::FontId::proportional(14.0),
            ui.visuals().weak_text_color(),
        );
        return action;
    }

    let mut list_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(list_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    list_ui.set_width(style.width);

    let content_height = projects.len() as f32 * (style.row_height + 1.0);
    if content_height <= list_rect.height() {
        render_rows(
            &mut list_ui,
            projects,
            active_id,
            style,
            catalog,
            &mut action,
        );
    } else {
        egui::ScrollArea::vertical()
            .id_salt("env_project_list_scroll")
            .auto_shrink([false, false])
            .show(&mut list_ui, |ui| {
                render_rows(ui, projects, active_id, style, catalog, &mut action);
            });
    }

    action
}

fn render_rows(
    ui: &mut egui::Ui,
    projects: &[EnvProjectRow],
    active_id: &str,
    style: &EnvProjectListStyle,
    catalog: &i18n::Catalog,
    action: &mut EnvProjectListAction,
) {
    ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
    for project in projects {
        let selected = project.id == active_id;
        let next = render_row(ui, project, selected, projects.len(), style, catalog);
        if !matches!(next, EnvProjectListAction::None) {
            *action = next;
        }
        row_divider(ui, style.width);
    }
}

fn paint_header(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    style: &EnvProjectListStyle,
    catalog: &i18n::Catalog,
) -> bool {
    // 다른 상세 페이지 제목(page_title: 15px strong)과 동일 속성(사용자 2026-07-09).
    ui.painter().text(
        egui::pos2(rect.left() + style.padding_x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        catalog.t("env.projects", &[]),
        egui::FontId::new(15.0, egui::FontFamily::Proportional),
        ui.visuals().text_color(),
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
        ui.visuals().widgets.hovered.weak_bg_fill
    } else {
        ui.visuals().extreme_bg_color
    };
    ui.painter().rect_filled(add_rect, 0.0, add_fill);
    ui.painter().rect_stroke(
        add_rect,
        0.0,
        ui.visuals().widgets.noninteractive.bg_stroke,
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        add_rect.center(),
        egui::Align2::CENTER_CENTER,
        "+",
        egui::FontId::proportional(16.0),
        ui.visuals().text_color(),
    );
    add.clicked()
}

fn render_row(
    ui: &mut egui::Ui,
    project: &EnvProjectRow,
    selected: bool,
    project_count: usize,
    style: &EnvProjectListStyle,
    catalog: &i18n::Catalog,
) -> EnvProjectListAction {
    let bg = panel_bg(ui);
    // 행 폭은 실제 가용 폭(스크롤바 예약 반영) — style.width 고정이면 우측 빈 띠,
    // max(style.width)면 좁은 뷰포트에서 스크롤바 침범(codex Low).
    let row_w = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(row_w, style.row_height), egui::Sense::click());
    let fill = if selected {
        tok_nav_active(ui)
    } else if response.hovered() {
        ui.visuals().widgets.hovered.weak_bg_fill
    } else {
        bg
    };

    let painter = ui.painter();
    // 선택/hover 배경은 아래 divider(1px)까지 포함해 세로로 빈틈없이 칠한다.
    let fill_rect =
        egui::Rect::from_min_max(rect.min, egui::pos2(rect.right(), rect.bottom() + 1.0));
    painter.rect_filled(fill_rect, 0.0, fill);

    // 삭제 버튼은 이름/env count가 있는 위쪽 줄과 세로 중심을 맞춘다.
    let delete_top = rect.center().y - style.row_text_offset_y - style.delete_button_height / 2.0;
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
    let name_pos = egui::pos2(
        rect.left() + style.padding_x,
        rect.center().y - style.row_text_offset_y,
    );
    let name_font = egui::FontId::proportional(style.name_font_size);
    let name_color = ui.visuals().text_color();
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
    painter.with_clip_rect(path_clip).text(
        egui::pos2(
            rect.left() + style.padding_x,
            rect.center().y + style.row_text_offset_y,
        ),
        egui::Align2::LEFT_CENTER,
        path,
        egui::FontId::proportional(style.path_font_size),
        ui.visuals().weak_text_color(),
    );

    painter.text(
        egui::pos2(count_right, rect.center().y - style.row_text_offset_y),
        egui::Align2::RIGHT_CENTER,
        format!("{}env", project.env_count),
        egui::FontId::proportional(style.count_font_size),
        ui.visuals().weak_text_color(),
    );
    painter.text(
        egui::pos2(count_right, rect.center().y + style.row_text_offset_y),
        egui::Align2::RIGHT_CENTER,
        format!("{}key", project.key_count),
        egui::FontId::proportional(style.count_font_size),
        ui.visuals().weak_text_color(),
    );

    // 삭제 ×는 마우스가 행 위에 있을 때만 — 벗어나면 사라진다(사용자 2026-07-09).
    // 키보드/터치 접근 경로 없음은 로컬 데스크톱(마우스) 전제로 수용(codex Low).
    if response.hovered() {
        let delete_enabled = !selected && project_count > 1;
        let delete = ui
            .interact(
                delete_rect,
                ui.id().with(("env_project_delete", &project.id)),
                egui::Sense::click(),
            )
            .on_hover_text(if selected {
                catalog.t("workspace.manager.delete_active_hint", &[])
            } else {
                catalog.t("action.delete", &[])
            });
        paint_delete_button(ui, delete_rect, delete.hovered() && delete_enabled);
        if delete.clicked() && delete_enabled {
            return EnvProjectListAction::DeleteRequested(project.id.clone());
        }
    }

    if response.clicked() && !selected {
        EnvProjectListAction::Select(project.id.clone())
    } else {
        EnvProjectListAction::None
    }
}

fn paint_delete_button(ui: &mut egui::Ui, rect: egui::Rect, danger: bool) {
    let fill = if danger {
        ui.visuals().error_fg_color
    } else {
        ui.visuals().window_fill
    };
    let stroke = if danger {
        ui.visuals().error_fg_color
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke.color
    };
    let text = if danger {
        ui.visuals().window_fill
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
        egui::FontId::proportional(12.0),
        text,
    );
}

fn row_divider(ui: &mut egui::Ui, width: f32) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let _ = width;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    paint_hline(ui.painter(), rect.x_range().into(), rect.center().y, color);
}

fn paint_hline(
    painter: &egui::Painter,
    x_range: std::ops::RangeInclusive<f32>,
    y: f32,
    color: egui::Color32,
) {
    let y = painter.round_to_pixel_center(y);
    painter.hline(x_range, y, egui::Stroke::new(1.0, color));
}

fn display_project_path(path: &str) -> String {
    if path.trim().is_empty() {
        return "~".to_owned();
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        let path_buf = std::path::PathBuf::from(path);
        if let Ok(stripped) = path_buf.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.to_owned()
}
