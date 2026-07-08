use crate::config::{Config, Theme};

const PAGE_TITLE_SIZE: f32 = 15.0;
const SECTION_TITLE_SIZE: f32 = 14.0;
const ROW_TITLE_SIZE: f32 = 14.0;
const ROW_DESC_SIZE: f32 = 13.0;
const CONTROL_TEXT_SIZE: f32 = 14.0;
const CONTROL_HEIGHT: f32 = 28.0;
const DETAIL_PAD_X: f32 = 10.0;

/// Remote(TLS) 섹션이 App에 돌려주는 동작 — App만 서버 핸들/파일을 소유하므로 의도만 전달한다.
pub enum RemoteAction {
    None,
    /// 체크 on — 서버 기동 요청.
    Start,
    /// 체크 off — 서버 정지 요청.
    Stop,
    /// known_hosts에서 이 host의 핀을 삭제(forget).
    Forget(String),
}

/// Remote 섹션 렌더에 필요한 현재 상태 (App이 채워 넘긴다 — UI는 서버를 직접 만지지 않는다).
pub struct RemoteView<'a> {
    /// 서버가 실행 중인가 — 체크박스 상태의 진실 소스(시작 실패 시 꺼진 채로 남는다).
    pub running: bool,
    /// 실행 중이면 bind 주소("127.0.0.1:포트").
    pub addr: Option<String>,
    /// 서버 신원 지문(SHA-256) — 클라이언트 TOFU 대조용.
    pub fingerprint: Option<&'a str>,
    /// 이번 실행의 attach 토큰(민감) — 기본 마스킹.
    pub token: Option<&'a str>,
    /// 시작 실패 등 표시할 에러.
    pub error: Option<&'a str>,
    /// known_hosts 파일 경로(안내 표시).
    pub known_hosts_path: String,
    /// known_hosts 항목 (host, 지문 full). 표시 시 지문은 잘라 보여준다.
    pub known_hosts: &'a [(String, String)],
}

/// 통합 설정 창의 좌측 네비 카테고리. 설정 5개는 이 파일이 인라인 렌더하고, 관리/모니터
/// 7개는 App이 `render_management` 콜백으로 각 패널의 contents()를 렌더한다 (2026-07-06 전체 통합).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Category {
    #[default]
    General,
    Language,
    Terminal,
    Performance,
    RemoteTls,
    // ── 관리 (App이 render_management로 렌더) ──
    Credentials,
    Connectors,
    Environment,
    Agents,
    Workspaces,
    // ── 모니터 ──
    Activity,
    Notifications,
}

/// 설정 창 결과.
pub struct SettingsOutput {
    /// config 값이 바뀌어 저장이 필요한가 (테마/터미널/성능/포트).
    pub config_changed: bool,
    /// Remote 섹션 동작 요청.
    pub remote_action: RemoteAction,
}

/// 통합 설정 창 (2026-07-06 — 흩어진 툴바 기능을 좌측 네비 한 창으로).
/// 좌측: 검색 + 그룹별 카테고리. 우측: 선택된 카테고리의 폼. 설정 5개는 여기서 인라인
/// 렌더하고, 관리/모니터 7개는 `render_management(ui, category)` 콜백으로 App이 각 패널의
/// contents()를 그린다.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    category: &mut Category,
    config: &mut Config,
    remote: &RemoteView,
    reveal_token: &mut bool,
    notif_unread: u32,
    search_query: &mut String,
    catalog: &i18n::Catalog,
    mut render_management: impl FnMut(&mut egui::Ui, Category),
) -> SettingsOutput {
    let mut changed = false;
    let mut remote_action = RemoteAction::None;

    // title_bar(false)라 기본 open 처리가 없다 — 닫힘이면 창 자체를 만들지 않는다.
    if !*open {
        return SettingsOutput {
            config_changed: false,
            remote_action,
        };
    }
    if *category == Category::Credentials {
        *category = Category::Environment;
    }

    // 별도 OS 창(immediate viewport) — 앱 창 안에 갇혀 있던 egui::Window를 네이티브 창으로
    // 전환해 메인 창 밖으로도 옮길 수 있게 한다(사용자 요청 2026-07-08). 타이틀/닫기는
    // 네이티브 타이틀바가 담당하므로 기존 커스텀 30px 타이틀 행은 제거.
    // 멀티뷰포트 미지원 백엔드에서는 egui가 자동으로 임베디드 창으로 폴백한다.
    ctx.show_viewport_immediate(
        egui::ViewportId::from_hash_of("deppy_settings_window"),
        egui::ViewportBuilder::default()
            .with_title(catalog.t("settings.title", &[]))
            .with_inner_size([1000.0, 640.0])
            .with_min_inner_size([720.0, 460.0]),
        // egui 0.35 immediate viewport 콜백은 &mut Ui(뷰포트 루트)를 받는다 — Context 아님.
        // Ui::input은 현재(자식) 뷰포트의 입력을 읽으므로 close_requested가 이 창의 것.
        |root, _class| {
            apply_settings_palette(root);
            let win_frame = egui::Frame::default()
                .inner_margin(egui::Margin::ZERO)
                .fill(root.visuals().window_fill);
            if root.input(|i| i.viewport().close_requested()) {
                *open = false;
            }
            egui::CentralPanel::default()
                .frame(win_frame)
                .show(root, |ui| {
                    let nav_frame = egui::Frame::default()
                        .fill(ui.visuals().faint_bg_color) // panel2 — 우측 폼과 톤 분리 (#72)
                        // 좌측 여백 축소(#1) — 창 여백 0과 합쳐 네비가 창 왼쪽에 밀착.
                        .inner_margin(egui::Margin {
                            left: 8,
                            right: 8,
                            top: 10,
                            bottom: 10,
                        });
                    egui::Panel::left("settings_nav")
                        .resizable(false)
                        .exact_size(216.0)
                        .frame(nav_frame)
                        .show(ui, |ui| {
                            nav(ui, category, notif_unread, search_query, catalog);
                        });
                    let detail_frame = egui::Frame::default()
                        .fill(ui.visuals().faint_bg_color)
                        .inner_margin(egui::Margin::ZERO);
                    egui::CentralPanel::default()
                        .frame(detail_frame)
                        .show(ui, |ui| {
                            let mut render_detail = |ui: &mut egui::Ui| match *category {
                                Category::General => {
                                    general_page(ui, config, &mut changed, catalog)
                                }
                                Category::Language => {
                                    language_page(ui, config, &mut changed, catalog)
                                }
                                Category::Terminal => {
                                    terminal_page(ui, config, &mut changed, catalog)
                                }
                                Category::Performance => {
                                    performance_page(ui, config, &mut changed, catalog)
                                }
                                Category::RemoteTls => remote_page(
                                    ui,
                                    config,
                                    remote,
                                    reveal_token,
                                    &mut changed,
                                    &mut remote_action,
                                    catalog,
                                ),
                                other => render_management(ui, other),
                            };

                            apply_component_style(ui);
                            if matches!(*category, Category::Environment) {
                                render_detail(ui);
                            } else {
                                egui::ScrollArea::vertical()
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        apply_component_style(ui);
                                        render_detail(ui);
                                    });
                            }
                        });
                });
        },
    );

    SettingsOutput {
        config_changed: changed,
        remote_action,
    }
}

fn rgb(r: u8, g: u8, b: u8) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}

/// `design/.../egui.ts`의 토큰을 설정 창 local visuals에 매핑한다.
fn apply_settings_palette(ui: &mut egui::Ui) {
    let dark = ui.visuals().dark_mode;
    let (bg, surface, surface_hover, panel, border, border_focus, text, muted, accent, input) =
        if dark {
            (
                rgb(0x1a, 0x1a, 0x1a),
                rgb(0x24, 0x24, 0x24),
                rgb(0x2c, 0x2c, 0x2c),
                rgb(0x20, 0x20, 0x20),
                rgb(0x3a, 0x3a, 0x3a),
                rgb(0x5a, 0x9f, 0xd4),
                rgb(0xd4, 0xd4, 0xd4),
                rgb(0x71, 0x71, 0x71),
                rgb(0x4d, 0xa6, 0xc8),
                rgb(0x1a, 0x1a, 0x1a),
            )
        } else {
            (
                rgb(0xe0, 0xe0, 0xe0),
                rgb(0xf0, 0xf0, 0xf0),
                rgb(0xe8, 0xe8, 0xe8),
                rgb(0xfa, 0xfa, 0xfa),
                rgb(0xc4, 0xc4, 0xc4),
                rgb(0x3a, 0x88, 0xbf),
                rgb(0x1a, 0x1a, 0x1a),
                rgb(0x88, 0x88, 0x88),
                rgb(0x3a, 0x88, 0xbf),
                rgb(0xff, 0xff, 0xff),
            )
        };

    let v = ui.visuals_mut();
    v.override_text_color = Some(text);
    v.panel_fill = surface;
    v.window_fill = bg;
    v.faint_bg_color = panel;
    v.extreme_bg_color = input;
    v.selection.bg_fill = accent;
    v.selection.stroke = egui::Stroke::new(1.0, border_focus);
    v.window_stroke = egui::Stroke::new(1.0, border);
    v.warn_fg_color = rgb(0xe7, 0x8a, 0x4e);
    v.error_fg_color = if dark {
        rgb(0xc8, 0x4d, 0x4d)
    } else {
        rgb(0xbf, 0x3a, 0x3a)
    };

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = surface;
    w.noninteractive.weak_bg_fill = panel;
    w.noninteractive.bg_stroke = egui::Stroke::new(1.0, border);
    w.noninteractive.fg_stroke = egui::Stroke::new(1.0, text);
    w.inactive.bg_fill = input;
    w.inactive.weak_bg_fill = input;
    w.inactive.bg_stroke = egui::Stroke::new(1.0, border);
    w.inactive.fg_stroke = egui::Stroke::new(1.0, muted);
    w.hovered.bg_fill = surface_hover;
    w.hovered.weak_bg_fill = surface_hover;
    w.hovered.bg_stroke = egui::Stroke::new(1.0, border);
    w.hovered.fg_stroke = egui::Stroke::new(1.0, text);
    w.active.bg_fill = surface_hover;
    w.active.weak_bg_fill = surface_hover;
    w.active.bg_stroke = egui::Stroke::new(1.0, border_focus);
    w.active.fg_stroke = egui::Stroke::new(1.0, text);
    w.open.bg_fill = surface_hover;
    w.open.weak_bg_fill = surface_hover;
    w.open.bg_stroke = egui::Stroke::new(1.0, border);
    w.open.fg_stroke = egui::Stroke::new(1.0, text);
}

/// 관리/모니터 패널의 버튼·입력을 설정 가이드의 sharp egui 룰로 통일한다.
/// 1px border, radius 0, control text 14px에 맞춘다.
fn apply_component_style(ui: &mut egui::Ui) {
    let spacing = ui.spacing_mut();
    spacing.interact_size.y = CONTROL_HEIGHT;
    spacing.button_padding = egui::vec2(10.0, 5.0);
    spacing.item_spacing = egui::vec2(8.0, 6.0);
    let input = ui.visuals().extreme_bg_color;
    let hover = ui.visuals().widgets.hovered.bg_fill;
    let stroke = ui.visuals().widgets.inactive.bg_stroke;
    let active_stroke = ui.visuals().selection.stroke;
    let radius = egui::CornerRadius::same(0);
    let v = ui.visuals_mut();
    v.widgets.inactive.weak_bg_fill = input;
    v.widgets.inactive.bg_fill = input;
    v.widgets.inactive.bg_stroke = stroke;
    v.widgets.inactive.corner_radius = radius;
    v.widgets.hovered.weak_bg_fill = hover;
    v.widgets.hovered.bg_fill = hover;
    v.widgets.hovered.bg_stroke = stroke;
    v.widgets.hovered.corner_radius = radius;
    v.widgets.active.weak_bg_fill = hover;
    v.widgets.active.bg_fill = hover;
    v.widgets.active.bg_stroke = active_stroke;
    v.widgets.active.corner_radius = radius;
}

// ── 좌측 네비 ──

fn nav(
    ui: &mut egui::Ui,
    category: &mut Category,
    notif_unread: u32,
    search_query: &mut String,
    catalog: &i18n::Catalog,
) {
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::singleline(search_query)
            .hint_text(catalog.t("settings.search", &[]))
            .margin(egui::Margin::symmetric(10, 9)) // 검색창 크기 (#72→키움)
            .desired_width(ui.available_width()),
    );
    ui.add_space(8.0);

    let query = search_query.trim().to_owned();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let query = query.as_str();
            let mut rendered = 0usize;

            let settings = [
                (
                    Category::General,
                    Icon::Gear,
                    catalog.t("settings.cat.general", &[]),
                    "general appearance theme folder tree sidebar ui",
                ),
                (
                    Category::Language,
                    Icon::Globe,
                    catalog.t("settings.language", &[]),
                    "language locale i18n english japanese chinese korean",
                ),
                (
                    Category::Terminal,
                    Icon::Terminal,
                    catalog.t("settings.terminal", &[]),
                    "terminal font scrollback shell paste clipboard",
                ),
                (
                    Category::Performance,
                    Icon::Bolt,
                    catalog.t("settings.performance", &[]),
                    "performance output batch cpu memory rss resource",
                ),
                (
                    Category::RemoteTls,
                    Icon::Lock,
                    catalog.t("settings.remote_tls", &[]),
                    "remote tls server port token fingerprint known hosts",
                ),
            ];
            let visible_settings: Vec<_> = settings
                .into_iter()
                .filter(|(_, _, label, aliases)| nav_matches(query, label, aliases))
                .collect();
            if !visible_settings.is_empty() {
                nav_group_label(ui, &catalog.t("settings.group.settings", &[]));
                for (cat, icon, label, _) in visible_settings {
                    rendered += 1;
                    nav_item(ui, category, cat, icon, &label, None);
                }
            }

            let manage = [
                (
                    Category::Connectors,
                    Icon::Link,
                    catalog.t("top.connectors", &[]),
                    "connectors mcp tools oauth server",
                ),
                (
                    Category::Environment,
                    Icon::Grid,
                    catalog.t("top.environment", &[]),
                    "environment env profile variables production credentials secrets api key token password",
                ),
                (
                    Category::Agents,
                    Icon::Diamond,
                    catalog.t("top.agents", &[]),
                    "agents command runner status regex",
                ),
                (
                    Category::Workspaces,
                    Icon::Square,
                    catalog.t("top.workspaces", &[]),
                    "workspaces project path folder root",
                ),
            ];
            let visible_manage: Vec<_> = manage
                .into_iter()
                .filter(|(_, _, label, aliases)| nav_matches(query, label, aliases))
                .collect();
            if !visible_manage.is_empty() {
                ui.add_space(6.0);
                nav_group_label(ui, &catalog.t("settings.group.manage", &[]));
                for (cat, icon, label, _) in visible_manage {
                    rendered += 1;
                    nav_item(ui, category, cat, icon, &label, None);
                }
            }

            let badge = (notif_unread > 0).then(|| notif_unread.to_string());
            let monitor = [
                (
                    Category::Activity,
                    Icon::Clock,
                    catalog.t("top.activity", &[]),
                    "activity monitor cpu rss memory process workspace backpressure",
                ),
                (
                    Category::Notifications,
                    Icon::Bell,
                    catalog.t("top.notifications", &[]),
                    "notifications alerts unread status approval",
                ),
            ];
            let visible_monitor: Vec<_> = monitor
                .into_iter()
                .filter(|(_, _, label, aliases)| nav_matches(query, label, aliases))
                .collect();
            if !visible_monitor.is_empty() {
                ui.add_space(6.0);
                nav_group_label(ui, &catalog.t("settings.group.monitor", &[]));
                for (cat, icon, label, _) in visible_monitor {
                    rendered += 1;
                    let item_badge = (cat == Category::Notifications)
                        .then(|| badge.clone())
                        .flatten();
                    nav_item(ui, category, cat, icon, &label, item_badge);
                }
            }

            if rendered == 0 {
                nav_group_label(ui, &catalog.t("settings.search.no_results", &[]));
            }
        });
}

fn nav_group_label(ui: &mut egui::Ui, label: &str) {
    ui.label(
        egui::RichText::new(label.to_ascii_uppercase())
            .weak()
            .size(13.0),
    );
}

fn nav_item(
    ui: &mut egui::Ui,
    current: &mut Category,
    cat: Category,
    icon: Icon,
    label: &str,
    badge: Option<String>,
) {
    if nav_row(ui, *current == cat, icon, label, badge) {
        *current = cat;
    }
}

/// 전체폭 네비 항목 — 아이콘 + 라벨, 선택/hover 배경이 행 전체를 덮는다 (목업 §설정).
/// 선택은 accent-soft, 아이콘·라벨은 accent/dim. 반환: 클릭 여부.
fn nav_row(
    ui: &mut egui::Ui,
    selected: bool,
    icon: Icon,
    label: &str,
    badge: Option<String>,
) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::click());
    let accent = ui.visuals().selection.bg_fill;
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
        p.rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
    } else if resp.hovered() {
        p.rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let icon_col = if selected {
        accent
    } else {
        ui.visuals().weak_text_color()
    };
    let cy = rect.center().y;
    paint_icon(p, egui::pos2(rect.left() + 14.0, cy), 15.0, icon, icon_col);
    let tc = if selected {
        accent
    } else {
        ui.visuals().text_color()
    };
    p.text(
        egui::pos2(rect.left() + 32.0, cy),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        tc,
    );
    if let Some(b) = badge {
        let br = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 16.0, cy),
            egui::vec2(18.0, 16.0),
        );
        p.rect_filled(br, 0.0, accent);
        p.text(
            br.center(),
            egui::Align2::CENTER_CENTER,
            b,
            egui::FontId::proportional(11.0),
            egui::Color32::WHITE,
        );
    }
    resp.clicked()
}

fn nav_matches(query: &str, label: &str, aliases: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    label.to_lowercase().contains(&query) || aliases.to_lowercase().contains(&query)
}

// ── 폼 헬퍼 ──

/// 설정 상세 페이지 제목 — Settings Detail Font Map 기준 15px.
fn page_title(ui: &mut egui::Ui, title: &str) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.add_space(DETAIL_PAD_X);
        ui.label(egui::RichText::new(title).size(PAGE_TITLE_SIZE).strong());
    });
    ui.add_space(8.0);
    crate::ui::hairline_full(ui);
}

/// 섹션 제목 — 카드 없이 title + 1px divider만 사용한다.
fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.add_space(DETAIL_PAD_X);
        ui.label(egui::RichText::new(title).size(SECTION_TITLE_SIZE).strong());
    });
    ui.add_space(4.0);
    crate::ui::hairline_full(ui);
}

/// label(+hint) 왼쪽, 컨트롤 오른쪽. 아래 픽셀-스냅 헤어라인.
fn row(
    ui: &mut egui::Ui,
    label: &str,
    hint: Option<&str>,
    add_control: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(7.0);
    let row_width = (ui.available_width() - DETAIL_PAD_X * 2.0).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(DETAIL_PAD_X);
        ui.allocate_ui_with_layout(
            egui::vec2(row_width, 0.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_width(row_width);
                ui.vertical(|ui| {
                    ui.set_width((row_width * 0.58).min(440.0));
                    ui.label(egui::RichText::new(label).size(ROW_TITLE_SIZE));
                    if let Some(h) = hint {
                        ui.label(egui::RichText::new(h).weak().size(ROW_DESC_SIZE));
                    }
                });
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    add_control,
                );
            },
        );
    });
    ui.add_space(7.0);
    crate::ui::hairline_full(ui);
}

fn detail_text(ui: &mut egui::Ui, text: impl Into<String>, selectable: bool) {
    let label = egui::Label::new(
        egui::RichText::new(text.into())
            .monospace()
            .size(CONTROL_TEXT_SIZE),
    )
    .selectable(selectable)
    .wrap();
    ui.add(label);
}

fn hint_text(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(egui::RichText::new(text.into()).weak().size(ROW_DESC_SIZE));
}

fn detail_block(ui: &mut egui::Ui, label: &str, value: impl Into<String>) {
    ui.add_space(7.0);
    ui.horizontal(|ui| {
        ui.add_space(DETAIL_PAD_X);
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - DETAIL_PAD_X).max(0.0));
            ui.label(egui::RichText::new(label).size(ROW_TITLE_SIZE));
            ui.add_space(3.0);
            detail_text(ui, value, true);
        });
    });
    ui.add_space(7.0);
    crate::ui::hairline_full(ui);
}

/// 토글 스위치 (checkbox 대체 — 목업 스타일). 값이 바뀌면 true.
fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(46.0, 26.0), egui::Sense::click());
    let mut changed = false;
    if resp.clicked() {
        *on = !*on;
        changed = true;
    }
    let radius = 2.0;
    let bg = if *on {
        ui.visuals().selection.bg_fill
    } else {
        ui.visuals().widgets.inactive.bg_fill
    };
    let stroke = if *on {
        ui.visuals().selection.stroke
    } else {
        ui.visuals().widgets.inactive.bg_stroke
    };
    let painter = ui.painter();
    painter.rect(rect, radius, bg, stroke, egui::StrokeKind::Inside);
    let t = if *on { 1.0 } else { 0.0 };
    let knob_size = egui::vec2(18.0, 18.0);
    let knob_left = egui::lerp((rect.left() + 4.0)..=(rect.right() - 4.0 - knob_size.x), t);
    let knob = egui::Rect::from_min_size(
        egui::pos2(knob_left, rect.center().y - knob_size.y / 2.0),
        knob_size,
    );
    let knob_fill = if *on {
        egui::Color32::WHITE
    } else {
        ui.visuals().weak_text_color()
    };
    painter.rect_filled(knob, 2.0, knob_fill);
    changed
}

// ── 네비 아이콘 · 세그먼트 · 스텝퍼 (painter 도형 — 이모지 □ 깨짐 회피) ──

#[derive(Clone, Copy)]
pub enum Icon {
    Gear,
    Globe,
    Terminal,
    Bolt,
    Lock,
    Link,
    Grid,
    Diamond,
    Square,
    Clock,
    Bell,
    Monitor,
    Sun,
    Moon,
}

fn paint_icon(p: &egui::Painter, c: egui::Pos2, sz: f32, icon: Icon, col: egui::Color32) {
    let s = egui::Stroke::new(1.4, col);
    let r = sz / 2.0;
    match icon {
        Icon::Gear => {
            p.circle_stroke(c, r * 0.62, s);
            p.circle_filled(c, r * 0.22, col);
            for i in 0..6 {
                let a = i as f32 * std::f32::consts::TAU / 6.0;
                let (dx, dy) = (a.cos(), a.sin());
                p.line_segment(
                    [
                        egui::pos2(c.x + dx * r * 0.62, c.y + dy * r * 0.62),
                        egui::pos2(c.x + dx * r, c.y + dy * r),
                    ],
                    s,
                );
            }
        }
        Icon::Globe => {
            p.circle_stroke(c, r * 0.8, s);
            p.line_segment(
                [
                    egui::pos2(c.x - r * 0.8, c.y),
                    egui::pos2(c.x + r * 0.8, c.y),
                ],
                s,
            );
            let e = egui::Rect::from_center_size(c, egui::vec2(r * 0.8, r * 1.6));
            p.rect_stroke(e, r * 0.4, s, egui::StrokeKind::Inside);
        }
        Icon::Terminal => {
            let b = egui::Rect::from_center_size(c, egui::vec2(sz, sz * 0.82));
            p.rect_stroke(b, 2.0, s, egui::StrokeKind::Inside);
            p.line_segment(
                [
                    egui::pos2(c.x - r * 0.5, c.y - r * 0.25),
                    egui::pos2(c.x - r * 0.1, c.y + r * 0.05),
                ],
                s,
            );
            p.line_segment(
                [
                    egui::pos2(c.x - r * 0.1, c.y + r * 0.05),
                    egui::pos2(c.x - r * 0.5, c.y + r * 0.35),
                ],
                s,
            );
        }
        Icon::Bolt => {
            p.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x + r * 0.2, c.y - r),
                    egui::pos2(c.x - r * 0.5, c.y + r * 0.15),
                    egui::pos2(c.x, c.y + r * 0.15),
                    egui::pos2(c.x - r * 0.2, c.y + r),
                    egui::pos2(c.x + r * 0.5, c.y - r * 0.15),
                    egui::pos2(c.x, c.y - r * 0.15),
                ],
                col,
                egui::Stroke::NONE,
            ));
        }
        Icon::Lock => {
            let body = egui::Rect::from_min_size(
                egui::pos2(c.x - r * 0.6, c.y - r * 0.1),
                egui::vec2(r * 1.2, r * 0.95),
            );
            p.rect_stroke(body, 2.0, s, egui::StrokeKind::Inside);
            p.add(egui::Shape::Path(egui::epaint::PathShape {
                points: vec![
                    egui::pos2(c.x - r * 0.35, c.y - r * 0.1),
                    egui::pos2(c.x - r * 0.35, c.y - r * 0.55),
                    egui::pos2(c.x + r * 0.35, c.y - r * 0.55),
                    egui::pos2(c.x + r * 0.35, c.y - r * 0.1),
                ],
                closed: false,
                fill: egui::Color32::TRANSPARENT,
                stroke: s.into(),
            }));
        }
        Icon::Link => {
            let a = egui::Rect::from_center_size(
                egui::pos2(c.x - r * 0.35, c.y - r * 0.35),
                egui::vec2(r * 0.9, r * 0.55),
            );
            let b = egui::Rect::from_center_size(
                egui::pos2(c.x + r * 0.35, c.y + r * 0.35),
                egui::vec2(r * 0.9, r * 0.55),
            );
            p.rect_stroke(a, r * 0.3, s, egui::StrokeKind::Inside);
            p.rect_stroke(b, r * 0.3, s, egui::StrokeKind::Inside);
        }
        Icon::Grid => {
            for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let cell = egui::Rect::from_center_size(
                    egui::pos2(c.x + dx * r * 0.42, c.y + dy * r * 0.42),
                    egui::vec2(r * 0.55, r * 0.55),
                );
                p.rect_stroke(cell, 1.0, s, egui::StrokeKind::Inside);
            }
        }
        Icon::Diamond => {
            p.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x, c.y - r * 0.85),
                    egui::pos2(c.x + r * 0.85, c.y),
                    egui::pos2(c.x, c.y + r * 0.85),
                    egui::pos2(c.x - r * 0.85, c.y),
                ],
                col,
                egui::Stroke::NONE,
            ));
        }
        Icon::Square => {
            let b = egui::Rect::from_center_size(c, egui::vec2(sz * 0.85, sz * 0.85));
            p.rect_stroke(b, 2.0, s, egui::StrokeKind::Inside);
        }
        Icon::Clock => {
            p.circle_stroke(c, r * 0.8, s);
            p.line_segment([c, egui::pos2(c.x, c.y - r * 0.5)], s);
            p.line_segment([c, egui::pos2(c.x + r * 0.4, c.y)], s);
        }
        Icon::Bell => {
            p.add(egui::Shape::Path(egui::epaint::PathShape {
                points: vec![
                    egui::pos2(c.x - r * 0.6, c.y + r * 0.4),
                    egui::pos2(c.x - r * 0.45, c.y - r * 0.2),
                    egui::pos2(c.x, c.y - r * 0.7),
                    egui::pos2(c.x + r * 0.45, c.y - r * 0.2),
                    egui::pos2(c.x + r * 0.6, c.y + r * 0.4),
                ],
                closed: true,
                fill: egui::Color32::TRANSPARENT,
                stroke: s.into(),
            }));
            p.circle_filled(egui::pos2(c.x, c.y + r * 0.65), r * 0.14, col);
        }
        Icon::Monitor => {
            let screen = egui::Rect::from_center_size(
                egui::pos2(c.x, c.y - r * 0.15),
                egui::vec2(sz, sz * 0.7),
            );
            p.rect_stroke(screen, 2.0, s, egui::StrokeKind::Inside);
            p.line_segment(
                [
                    egui::pos2(c.x - r * 0.4, c.y + r * 0.85),
                    egui::pos2(c.x + r * 0.4, c.y + r * 0.85),
                ],
                s,
            );
            p.line_segment(
                [
                    egui::pos2(c.x, screen.bottom()),
                    egui::pos2(c.x, c.y + r * 0.85),
                ],
                s,
            );
        }
        Icon::Sun => {
            p.circle_filled(c, r * 0.42, col);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let (dx, dy) = (a.cos(), a.sin());
                p.line_segment(
                    [
                        egui::pos2(c.x + dx * r * 0.62, c.y + dy * r * 0.62),
                        egui::pos2(c.x + dx * r, c.y + dy * r),
                    ],
                    s,
                );
            }
        }
        Icon::Moon => {
            p.circle_filled(c, r * 0.8, col);
            p.circle_filled(
                egui::pos2(c.x + r * 0.42, c.y - r * 0.25),
                r * 0.72,
                egui::Color32::from_rgb(0x22, 0x22, 0x2a),
            );
        }
    }
}

/// 세그먼트 토글 (테마: 시스템/라이트/다크) — 아이콘 + 라벨, 좌→우 고정 순서, 구분선,
/// 선택 accent-soft. painter로 직접 그려 layout(right_to_left) 영향을 안 받는다.
/// 반환: 변경 여부.
fn segmented(ui: &mut egui::Ui, sel: &mut Theme, items: &[(Theme, Icon, String)]) -> bool {
    let font = egui::FontId::proportional(CONTROL_TEXT_SIZE);
    let h = CONTROL_HEIGHT;
    let icon_sz = 14.0;
    let gap = 6.0;
    let pad = 12.0;
    let hair = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let input = ui.visuals().extreme_bg_color;
    let accent = ui.visuals().selection.bg_fill;
    let text_w: Vec<f32> = items
        .iter()
        .map(|(_, _, l)| {
            ui.painter()
                .layout_no_wrap(l.clone(), font.clone(), accent)
                .size()
                .x
        })
        .collect();
    let widths: Vec<f32> = text_w
        .iter()
        .map(|w| pad + icon_sz + gap + w + pad)
        .collect();
    let total: f32 = widths.iter().sum();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, h), egui::Sense::hover());
    ui.painter().rect(
        rect,
        0.0,
        input,
        egui::Stroke::new(1.0, hair),
        egui::StrokeKind::Inside,
    );
    let mut changed = false;
    let mut x = rect.left();
    for (i, (theme, icon, label)) in items.iter().enumerate() {
        let seg = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(widths[i], h));
        let on = *sel == *theme;
        if on {
            ui.painter().rect_filled(seg.shrink(1.0), 0.0, accent);
        }
        let col = if on {
            egui::Color32::WHITE
        } else {
            ui.visuals().weak_text_color()
        };
        let cy = seg.center().y;
        paint_icon(
            ui.painter(),
            egui::pos2(seg.left() + pad + icon_sz / 2.0, cy),
            icon_sz,
            *icon,
            col,
        );
        ui.painter().text(
            egui::pos2(seg.left() + pad + icon_sz + gap, cy),
            egui::Align2::LEFT_CENTER,
            label,
            font.clone(),
            col,
        );
        if i < items.len() - 1 {
            ui.painter().vline(
                seg.right(),
                (rect.top() + 5.0)..=(rect.bottom() - 5.0),
                egui::Stroke::new(1.0, hair),
            );
        }
        let r = ui.interact(seg, ui.id().with(("seg", i)), egui::Sense::click());
        if r.clicked() && !on {
            *sel = *theme;
            changed = true;
        }
        x += widths[i];
    }
    changed
}

/// 사용자 입력 스텝퍼 — [값] │ [−] │ [+], 경계선 박스. 반환: 변경 여부.
fn stepper(ui: &mut egui::Ui, value: &mut i64, step: i64, min: i64, max: i64, unit: &str) -> bool {
    let h = CONTROL_HEIGHT;
    let btn_w = 28.0;
    let val_w = 76.0;
    let total = val_w + btn_w * 2.0;
    let hair = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let input = ui.visuals().extreme_bg_color;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, h), egui::Sense::hover());
    ui.painter().rect(
        rect,
        0.0,
        input,
        egui::Stroke::new(1.0, hair),
        egui::StrokeKind::Inside,
    );
    let shown = if unit.is_empty() {
        comma(*value)
    } else {
        format!("{} {unit}", comma(*value))
    };
    ui.painter().text(
        egui::pos2(rect.left() + val_w / 2.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        shown,
        egui::FontId::monospace(CONTROL_TEXT_SIZE),
        ui.visuals().text_color(),
    );
    let x1 = rect.left() + val_w;
    let x2 = x1 + btn_w;
    ui.painter()
        .vline(x1, rect.y_range(), egui::Stroke::new(1.0, hair));
    ui.painter()
        .vline(x2, rect.y_range(), egui::Stroke::new(1.0, hair));
    let minus = egui::Rect::from_min_size(egui::pos2(x1, rect.top()), egui::vec2(btn_w, h));
    let plus = egui::Rect::from_min_size(egui::pos2(x2, rect.top()), egui::vec2(btn_w, h));
    let mr = ui.interact(
        minus,
        ui.id().with(("minus", min, max, unit)),
        egui::Sense::click(),
    );
    let pr = ui.interact(
        plus,
        ui.id().with(("plus", min, max, unit)),
        egui::Sense::click(),
    );
    if mr.hovered() {
        ui.painter()
            .rect_filled(minus, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    if pr.hovered() {
        ui.painter()
            .rect_filled(plus, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let fc = ui.visuals().weak_text_color();
    ui.painter().text(
        minus.center(),
        egui::Align2::CENTER_CENTER,
        "−",
        egui::FontId::proportional(15.0),
        fc,
    );
    ui.painter().text(
        plus.center(),
        egui::Align2::CENTER_CENTER,
        "+",
        egui::FontId::proportional(15.0),
        fc,
    );
    let mut changed = false;
    if mr.clicked() {
        *value = (*value - step).clamp(min, max);
        changed = true;
    }
    if pr.clicked() {
        *value = (*value + step).clamp(min, max);
        changed = true;
    }
    changed
}

/// stepper의 f32 변형 — 소수 step(폰트 0.5px 등). 정수값은 정수로, 아니면 소수 1자리 표시.
fn stepper_f32(ui: &mut egui::Ui, value: &mut f32, step: f32, min: f32, max: f32) -> bool {
    let h = CONTROL_HEIGHT;
    let btn_w = 28.0;
    let val_w = 76.0;
    let total = val_w + btn_w * 2.0;
    let hair = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let input = ui.visuals().extreme_bg_color;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, h), egui::Sense::hover());
    ui.painter().rect(
        rect,
        0.0,
        input,
        egui::Stroke::new(1.0, hair),
        egui::StrokeKind::Inside,
    );
    let shown = if (*value - value.round()).abs() < 1e-3 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.1}")
    };
    ui.painter().text(
        egui::pos2(rect.left() + val_w / 2.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        shown,
        egui::FontId::monospace(CONTROL_TEXT_SIZE),
        ui.visuals().text_color(),
    );
    let x1 = rect.left() + val_w;
    let x2 = x1 + btn_w;
    ui.painter()
        .vline(x1, rect.y_range(), egui::Stroke::new(1.0, hair));
    ui.painter()
        .vline(x2, rect.y_range(), egui::Stroke::new(1.0, hair));
    let minus = egui::Rect::from_min_size(egui::pos2(x1, rect.top()), egui::vec2(btn_w, h));
    let plus = egui::Rect::from_min_size(egui::pos2(x2, rect.top()), egui::vec2(btn_w, h));
    let mr = ui.interact(minus, ui.id().with("fstep_minus"), egui::Sense::click());
    let pr = ui.interact(plus, ui.id().with("fstep_plus"), egui::Sense::click());
    if mr.hovered() {
        ui.painter()
            .rect_filled(minus, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    if pr.hovered() {
        ui.painter()
            .rect_filled(plus, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let fc = ui.visuals().weak_text_color();
    ui.painter().text(
        minus.center(),
        egui::Align2::CENTER_CENTER,
        "−",
        egui::FontId::proportional(15.0),
        fc,
    );
    ui.painter().text(
        plus.center(),
        egui::Align2::CENTER_CENTER,
        "+",
        egui::FontId::proportional(15.0),
        fc,
    );
    let mut changed = false;
    if mr.clicked() {
        *value = (*value - step).clamp(min, max);
        changed = true;
    }
    if pr.clicked() {
        *value = (*value + step).clamp(min, max);
        changed = true;
    }
    changed
}

fn comma(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

// ── 카테고리 페이지 ──

fn general_page(
    ui: &mut egui::Ui,
    config: &mut Config,
    changed: &mut bool,
    catalog: &i18n::Catalog,
) {
    page_title(ui, &catalog.t("settings.appearance", &[]));
    row(
        ui,
        &catalog.t("settings.theme", &[]),
        Some(&catalog.t("settings.theme.hint", &[])),
        |ui| {
            let items = [
                (
                    Theme::System,
                    Icon::Monitor,
                    catalog.t("settings.theme.system", &[]),
                ),
                (
                    Theme::Light,
                    Icon::Sun,
                    catalog.t("settings.theme.light", &[]),
                ),
                (
                    Theme::Dark,
                    Icon::Moon,
                    catalog.t("settings.theme.dark", &[]),
                ),
            ];
            *changed |= segmented(ui, &mut config.ui.theme, &items);
        },
    );
    row(
        ui,
        &catalog.t("settings.ui_font", &[]),
        Some(&catalog.t("settings.ui_font.hint", &[])),
        |ui| {
            // 시스템 폰트 스캔은 파일 IO — 프로세스당 1회 캐시(새 폰트는 재시작 후 표시).
            static FONT_OPTIONS: std::sync::LazyLock<Vec<(String, String)>> =
                std::sync::LazyLock::new(crate::fonts::ui_font_options);
            let current_label = config
                .ui
                .ui_font
                .as_deref()
                .and_then(|p| {
                    FONT_OPTIONS
                        .iter()
                        .find(|(_, path)| path == p)
                        .map(|(name, _)| name.clone())
                })
                .unwrap_or_else(|| catalog.t("settings.ui_font.auto", &[]));
            // 언어 콤보와 동일한 커스텀 박스(중앙 텍스트 + ▾ 도형) — 기본 ComboBox는
            // 스타일이 달라 컴포넌트가 튀었다(사용자 #5).
            let w = 180.0;
            let h = CONTROL_HEIGHT;
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
            let hair = ui.visuals().widgets.noninteractive.bg_stroke.color;
            let bg = if resp.hovered() {
                ui.visuals().widgets.hovered.bg_fill
            } else {
                ui.visuals().extreme_bg_color
            };
            ui.painter().rect(
                rect,
                0.0,
                bg,
                egui::Stroke::new(1.0, hair),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                current_label,
                egui::FontId::proportional(CONTROL_TEXT_SIZE),
                ui.visuals().text_color(),
            );
            {
                let ax = rect.right() - 12.0;
                let cy = rect.center().y;
                let d = 3.5;
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        egui::pos2(ax - d, cy - d * 0.6),
                        egui::pos2(ax + d, cy - d * 0.6),
                        egui::pos2(ax, cy + d * 0.7),
                    ],
                    ui.visuals().weak_text_color(),
                    egui::Stroke::NONE,
                ));
            }
            egui::Popup::menu(&resp).show(|ui| {
                ui.set_min_width(w);
                if ui
                    .selectable_label(
                        config.ui.ui_font.is_none(),
                        catalog.t("settings.ui_font.auto", &[]),
                    )
                    .clicked()
                {
                    config.ui.ui_font = None;
                    *changed = true;
                }
                for (name, path) in FONT_OPTIONS.iter() {
                    let selected = config.ui.ui_font.as_deref() == Some(path.as_str());
                    if ui.selectable_label(selected, name).clicked() {
                        config.ui.ui_font = Some(path.clone());
                        *changed = true;
                    }
                }
            });
        },
    );
    row(
        ui,
        &catalog.t("settings.file_tree_sidebar", &[]),
        Some(&catalog.t("settings.file_tree.hint", &[])),
        |ui| {
            if toggle_switch(ui, &mut config.ui.file_tree_enabled) {
                *changed = true;
            }
        },
    );
    row(
        ui,
        &catalog.t("settings.auto_resume", &[]),
        Some(&catalog.t("settings.auto_resume.hint", &[])),
        |ui| {
            if toggle_switch(ui, &mut config.ui.auto_resume_agents) {
                *changed = true;
            }
        },
    );
    row(
        ui,
        &catalog.t("settings.status_hooks", &[]),
        Some(&catalog.t("settings.status_hooks.hint", &[])),
        |ui| {
            if toggle_switch(ui, &mut config.ui.agent_status_hooks) {
                *changed = true;
            }
        },
    );
}

fn language_page(
    ui: &mut egui::Ui,
    config: &mut Config,
    changed: &mut bool,
    catalog: &i18n::Catalog,
) {
    page_title(ui, &catalog.t("settings.language", &[]));
    row(
        ui,
        &catalog.t("settings.locale", &[]),
        Some(&catalog.t("settings.locale.hint", &[])),
        |ui| {
            // egui ComboBox는 selected_text를 좌측정렬(하드코딩)이라 텍스트 중앙정렬이 안 된다
            // → 커스텀 박스(중앙 텍스트 + ▾) + Popup::menu로 구현한다(#6).
            let w = 130.0;
            let h = CONTROL_HEIGHT;
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
            let hair = ui.visuals().widgets.noninteractive.bg_stroke.color;
            let bg = if resp.hovered() {
                ui.visuals().widgets.hovered.bg_fill
            } else {
                ui.visuals().extreme_bg_color
            };
            ui.painter().rect(
                rect,
                0.0,
                bg,
                egui::Stroke::new(1.0, hair),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                current_locale_label(&config.i18n.locale, catalog),
                egui::FontId::proportional(CONTROL_TEXT_SIZE),
                ui.visuals().text_color(),
            );
            // 아래 화살표 — ▾ 문자는 폰트에 없어 □로 깨진다(사용자). 도형 삼각형으로 그린다.
            {
                let ax = rect.right() - 12.0;
                let cy = rect.center().y;
                let d = 3.5;
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        egui::pos2(ax - d, cy - d * 0.6),
                        egui::pos2(ax + d, cy - d * 0.6),
                        egui::pos2(ax, cy + d * 0.7),
                    ],
                    ui.visuals().weak_text_color(),
                    egui::Stroke::NONE,
                ));
            }
            egui::Popup::menu(&resp).show(|ui| {
                ui.set_min_width(w);
                for (locale, key) in [
                    (i18n::FALLBACK_LOCALE, "settings.locale.en_us"),
                    ("ja-JP", "settings.locale.ja_jp"),
                    ("zh-Hans", "settings.locale.zh_hans"),
                    ("zh-Hant", "settings.locale.zh_hant"),
                    ("ko-KR", "settings.locale.ko_kr"),
                ] {
                    if ui
                        .selectable_label(config.i18n.locale == locale, catalog.t(key, &[]))
                        .clicked()
                    {
                        config.i18n.locale = locale.to_owned();
                        *changed = true;
                    }
                }
            });
        },
    );
}

fn current_locale_label(locale: &str, catalog: &i18n::Catalog) -> String {
    let key = match locale {
        "ja-JP" => "settings.locale.ja_jp",
        "zh-Hans" => "settings.locale.zh_hans",
        "zh-Hant" => "settings.locale.zh_hant",
        "ko-KR" => "settings.locale.ko_kr",
        _ => "settings.locale.en_us",
    };
    catalog.t(key, &[])
}

fn terminal_page(
    ui: &mut egui::Ui,
    config: &mut Config,
    changed: &mut bool,
    catalog: &i18n::Catalog,
) {
    page_title(ui, &catalog.t("settings.terminal", &[]));
    row(ui, &catalog.t("settings.font_size", &[]), None, |ui| {
        let mut v = config.terminal.font_size;
        if stepper_f32(ui, &mut v, 0.5, 8.0, 32.0) {
            config.terminal.font_size = v;
            *changed = true;
        }
    });
    row(
        ui,
        &catalog.t("settings.scrollback_lines", &[]),
        Some(&catalog.t("settings.scrollback.hint", &[])),
        |ui| {
            let mut v = config.terminal.scrollback_lines as i64;
            if stepper(ui, &mut v, 500, 1_000, 100_000, "") {
                config.terminal.scrollback_lines = v as u32;
                *changed = true;
            }
        },
    );
}

fn performance_page(
    ui: &mut egui::Ui,
    config: &mut Config,
    changed: &mut bool,
    catalog: &i18n::Catalog,
) {
    page_title(ui, &catalog.t("settings.performance", &[]));
    row(
        ui,
        &catalog.t("settings.output_batch_ms", &[]),
        Some(&catalog.t("settings.output_batch.hint", &[])),
        |ui| {
            let mut v = config.performance.output_batch_ms as i64;
            if stepper(ui, &mut v, 1, 16, 50, "ms") {
                config.performance.output_batch_ms = v as u64;
                *changed = true;
            }
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn remote_page(
    ui: &mut egui::Ui,
    config: &mut Config,
    remote: &RemoteView,
    reveal_token: &mut bool,
    changed: &mut bool,
    remote_action: &mut RemoteAction,
    catalog: &i18n::Catalog,
) {
    page_title(ui, &catalog.t("settings.remote_tls", &[]));
    // 체크박스 = 실행 중 OR 저장된 자동시작 의도 (codex Medium — running만 반영하면
    // 실패 상태에서 auto-start를 UI로 끌 수 없음).
    let mut enabled = remote.running || config.remote.tls_enabled;
    row(
        ui,
        &catalog.t("settings.remote_tls_enabled", &[]),
        Some(&catalog.t("settings.toggle_restart_required", &[])),
        |ui| {
            if toggle_switch(ui, &mut enabled) {
                *remote_action = if enabled {
                    RemoteAction::Start
                } else {
                    RemoteAction::Stop
                };
            }
        },
    );
    row(ui, &catalog.t("settings.port", &[]), None, |ui| {
        *changed |= ui
            .add(egui::DragValue::new(&mut config.remote.port).range(0..=65535))
            .changed();
    });
    if let Some(err) = remote.error {
        ui.add_space(7.0);
        ui.colored_label(
            ui.visuals().error_fg_color,
            egui::RichText::new(catalog.t("settings.start_failed", &[("message", err)]))
                .size(ROW_DESC_SIZE),
        );
        ui.add_space(7.0);
        crate::ui::hairline(ui);
    }
    if remote.running {
        if let Some(addr) = &remote.addr {
            row(ui, &catalog.t("settings.address", &[]), None, |ui| {
                detail_text(ui, addr.as_str(), true);
            });
        }
        if let Some(fp) = remote.fingerprint {
            detail_block(ui, &catalog.t("settings.fingerprint", &[]), fp);
        }
        if let Some(token) = remote.token {
            row(ui, &catalog.t("settings.token", &[]), None, |ui| {
                ui.checkbox(reveal_token, catalog.t("settings.show", &[]));
            });
            if *reveal_token {
                detail_text(ui, token, true);
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    egui::RichText::new(catalog.t("settings.token_sensitive_warning", &[]))
                        .size(ROW_DESC_SIZE),
                );
            } else {
                hint_text(ui, catalog.t("settings.token_hidden_hint", &[]));
            }
        }
        hint_text(ui, catalog.t("settings.client_fingerprint_hint", &[]));
    }

    section(ui, &catalog.t("settings.known_hosts", &[]));
    hint_text(ui, remote.known_hosts_path.as_str());
    if remote.known_hosts.is_empty() {
        hint_text(ui, catalog.t("settings.no_trust_records", &[]));
    } else {
        for (host, fp) in remote.known_hosts {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(host)
                        .monospace()
                        .size(CONTROL_TEXT_SIZE),
                );
                ui.label(
                    egui::RichText::new(truncate_fingerprint(fp, 17))
                        .weak()
                        .size(ROW_DESC_SIZE),
                );
                if ui.button(catalog.t("settings.forget", &[])).clicked() {
                    *remote_action = RemoteAction::Forget(host.clone());
                }
            });
        }
    }
}

/// 지문을 목록 표시용으로 앞 `keep`자만 남기고 자른다(전체는 실행 중 서버 지문에서 확인).
/// char 경계 기준이라 비ASCII가 섞여도 패닉하지 않는다.
fn truncate_fingerprint(fp: &str, keep: usize) -> String {
    match fp.char_indices().nth(keep) {
        Some((idx, _)) => format!("{}…", &fp[..idx]),
        None => fp.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{nav_matches, truncate_fingerprint};

    #[test]
    fn 지문_짧으면_그대로() {
        assert_eq!(truncate_fingerprint("ab:cd", 10), "ab:cd");
    }

    #[test]
    fn 지문_길면_앞부분만_말줄임() {
        assert_eq!(truncate_fingerprint("aa:bb:cc:dd", 5), "aa:bb…");
    }

    #[test]
    fn 설정_검색은_label과_alias를_모두_본다() {
        assert!(nav_matches("term", "터미널", "terminal paste clipboard"));
        assert!(nav_matches("알림", "알림", "notifications alerts"));
        assert!(!nav_matches(
            "missing",
            "터미널",
            "terminal paste clipboard"
        ));
    }
}
