#[derive(Debug, Clone)]
pub struct EnvProjectRow {
    pub id: String,
    pub name: String,
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

    // 행/divider/선택 배경 폭은 전부 panel_rect(=패널 배경·헤더와 동일) 기준으로
    // 통일한다. ui.available_width()는 ScrollArea 유무/레이아웃에 따라 달라져
    // 우측 경계선 침범 또는 빈 여백 띠의 원인이 됐다(2026-07-10).
    let row_width = list_rect.width();
    let content_height = projects.len() as f32 * (style.row_height + 1.0);
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
                render_rows(ui, projects, active_id, row_width, style, catalog, &mut action);
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
        let next = render_row(ui, project, selected, projects.len(), row_width, style, catalog);
        if !matches!(next, EnvProjectListAction::None) {
            *action = next;
        }
        row_divider(ui, row_width);
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
    row_width: f32,
    style: &EnvProjectListStyle,
    catalog: &i18n::Catalog,
) -> EnvProjectListAction {
    let bg = panel_bg(ui);
    // 행 폭은 render_with_style이 잰 panel_rect 폭(row_width) 고정 — 패널 배경·
    // 헤더·divider와 우측 끝이 항상 일치한다. 스크롤바는 floating(예약 폭 0)이라
    // ScrollArea 유무와 무관하게 같은 폭이 유지된다(2026-07-10).
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(row_width, style.row_height), egui::Sense::click());
    let fill = if selected {
        tok_nav_active(ui)
    } else if response.hovered() {
        ui.visuals().widgets.hovered.weak_bg_fill
    } else {
        bg
    };

    let painter = ui.painter();
    // 선택/hover 배경은 위 divider(이전 행 경계)와 아래 divider까지 포함해
    // 세로로 빈틈없이 칠한다(2026-07-10: 상하 밝은 줄 제거).
    let fill_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top() - 1.0),
        egui::pos2(rect.right(), rect.bottom() + 1.0),
    );
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
    // 경로가 사라진 프로젝트(rename 자동 복구 불가 — EXDEV/셸 부재)는 경고색으로.
    let path_color = if project.path_missing {
        ui.visuals().error_fg_color
    } else {
        ui.visuals().weak_text_color()
    };
    painter.with_clip_rect(path_clip).text(
        egui::pos2(
            rect.left() + style.padding_x,
            rect.center().y + style.row_text_offset_y,
        ),
        egui::Align2::LEFT_CENTER,
        path,
        egui::FontId::proportional(style.path_font_size),
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
        // 선택(활성) 행에서도 삭제 요청을 발행한다 — 확인 다이얼로그·활성 워크스페이스
        // 전환은 App(오케스트레이터) 담당(2026-07-10). 마지막 1개 제한만 유지.
        let delete_enabled = project_count > 1;
        let delete = ui
            .interact(
                delete_rect,
                ui.id().with(("env_project_delete", &project.id)),
                egui::Sense::click(),
            )
            .on_hover_text(catalog.t("action.delete", &[]));
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
    // 행과 동일한 row_width를 그대로 사용 — available_width를 다시 재면 행 폭과
    // 어긋나 우측 끝이 들쭉날쭉해진다(2026-07-10).
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
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
