use crate::config::{Config, Theme};
use crate::shortcuts::{self, ShortcutAction, ShortcutGroup};

/// `SettingsDetailShell`/`SettingsRow`/control 목업이 공유하는 타이포 토큰.
/// 참조 화면의 Settings Detail Font Map(15/14/13/14px)을 한 곳에서 강제한다.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SettingsTypography {
    page_title: f32,
    section_title: f32,
    row_title: f32,
    row_description: f32,
    control: f32,
}

const SETTINGS_TYPE: SettingsTypography = SettingsTypography {
    page_title: 15.0,
    section_title: 14.0,
    row_title: 14.0,
    row_description: 13.0,
    control: 14.0,
};

/// 참조 React 컴포넌트의 logical px 치수.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SettingsDetailMetrics {
    pad_x: f32,
    pad_top: f32,
    pad_bottom: f32,
    row_height: f32,
    control_column: f32,
    column_gap: f32,
    control_height: f32,
    select_width: f32,
    segment_width: f32,
}

const SETTINGS_DETAIL: SettingsDetailMetrics = SettingsDetailMetrics {
    pad_x: 26.0,
    pad_top: 20.0,
    pad_bottom: 40.0,
    row_height: 68.0,
    control_column: 360.0,
    column_gap: 20.0,
    control_height: 34.0,
    select_width: 230.0,
    segment_width: 104.0,
};

const CONTROL_HEIGHT: f32 = SETTINGS_DETAIL.control_height;
const CONTROL_TEXT_SIZE: f32 = SETTINGS_TYPE.control;

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
    Shortcuts,
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
            // 참조 설정 창은 1280px 프레임에서 190px nav를 뺀 1090px 환경/API
            // surface다. 네이티브 타이틀바는 viewport 밖이므로 본문 높이는 730px.
            .with_inner_size([1280.0, 730.0])
            .with_min_inner_size([900.0, 520.0]),
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
                        .exact_size(190.0)
                        .frame(nav_frame)
                        .show(ui, |ui| {
                            nav(ui, category, notif_unread, search_query, catalog);
                        });
                    let inline_detail = is_inline_settings_category(*category);
                    let detail_frame = egui::Frame::default()
                        .fill(if inline_detail {
                            ui.visuals().extreme_bg_color
                        } else {
                            ui.visuals().faint_bg_color
                        })
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
                                Category::Shortcuts => {
                                    shortcuts_page(ui, config, &mut changed, catalog)
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
                                        if inline_detail {
                                            settings_detail_shell(ui, render_detail);
                                        } else {
                                            render_detail(ui);
                                        }
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

fn is_inline_settings_category(category: Category) -> bool {
    matches!(
        category,
        Category::General
            | Category::Language
            | Category::Terminal
            | Category::Shortcuts
            | Category::Performance
            | Category::RemoteTls
    )
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
    v.hyperlink_color = accent;
    // #d4d4d4 × 0.535 ≈ 목업 muted #717171.
    v.weak_text_alpha = 0.535;
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
                    Category::Shortcuts,
                    Icon::Keyboard,
                    catalog.t("settings.shortcuts", &[]),
                    "shortcuts keyboard key bindings hotkeys commands 단축키 키보드",
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

/// React의 `SettingsDetailShell`과 같은 공통 상세 surface.
/// 모든 인라인 설정 페이지가 동일한 배경·여백·수직 리듬을 공유한다.
fn settings_detail_shell(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: SETTINGS_DETAIL.pad_x as i8,
            right: SETTINGS_DETAIL.pad_x as i8,
            top: SETTINGS_DETAIL.pad_top as i8,
            bottom: SETTINGS_DETAIL.pad_bottom as i8,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // 행/구분선 자체가 참조의 간격을 포함하므로 egui의 암묵적 6px 간격은 제거한다.
            ui.spacing_mut().item_spacing.y = 0.0;
            add_contents(ui);
        });
}

fn settings_text_secondary(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        rgb(0xaa, 0xaa, 0xaa)
    } else {
        rgb(0x44, 0x44, 0x44)
    }
}

fn settings_input_border(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        rgb(0x40, 0x40, 0x40)
    } else {
        rgb(0xb8, 0xb8, 0xb8)
    }
}

fn settings_nav_active(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        rgb(0x2e, 0x4a, 0x5e)
    } else {
        rgb(0xcc, 0xde, 0xed)
    }
}

/// 내용 폭 안에서만 그리는 1px 구분선. 상세 페이지의 26px 좌우 여백을 침범하지 않는다.
fn settings_hairline(ui: &mut egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    let y = ui.painter().round_to_pixel_center(rect.center().y);
    ui.painter().hline(
        rect.x_range(),
        y,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
}

/// 설정 상세 페이지 제목 — Settings Detail Font Map 기준 15px.
fn page_title(ui: &mut egui::Ui, title: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), egui::Sense::hover());
    ui.painter().text(
        rect.left_center(),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(SETTINGS_TYPE.page_title),
        ui.visuals().strong_text_color(),
    );
    ui.add_space(16.0);
    settings_hairline(ui);
}

/// 섹션 제목 — 카드 없이 title + 1px divider만 사용한다.
fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(14.0);
    ui.label(
        egui::RichText::new(title)
            .size(SETTINGS_TYPE.section_title)
            .strong(),
    );
    ui.add_space(4.0);
    settings_hairline(ui);
}

/// React의 `SettingsRow`: 68px 행, `1fr 360px` 열, 20px gap, 아래 1px 구분선.
fn row(
    ui: &mut egui::Ui,
    label: &str,
    hint: Option<&str>,
    add_control: impl FnOnce(&mut egui::Ui),
) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), SETTINGS_DETAIL.row_height),
        egui::Sense::hover(),
    );
    let control_width = SETTINGS_DETAIL
        .control_column
        .min((rect.width() - SETTINGS_DETAIL.column_gap).max(0.0));
    let control_rect = egui::Rect::from_min_max(
        egui::pos2(rect.right() - control_width, rect.top()),
        rect.right_bottom(),
    );
    let label_rect = egui::Rect::from_min_max(
        rect.left_top(),
        egui::pos2(
            (control_rect.left() - SETTINGS_DETAIL.column_gap).max(rect.left()),
            rect.bottom(),
        ),
    );
    let painter = ui
        .painter()
        .with_clip_rect(label_rect.intersect(ui.clip_rect()));
    let title_y = if hint.is_some() {
        rect.top() + 21.0
    } else {
        rect.center().y
    };
    painter.text(
        egui::pos2(rect.left(), title_y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(SETTINGS_TYPE.row_title),
        ui.visuals().text_color(),
    );
    if let Some(hint) = hint {
        painter.text(
            egui::pos2(rect.left(), rect.top() + 46.0),
            egui::Align2::LEFT_CENTER,
            hint,
            egui::FontId::proportional(SETTINGS_TYPE.row_description),
            settings_text_secondary(ui),
        );
    }

    let mut control_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("settings_row_control", label))
            .max_rect(control_rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    control_ui.set_clip_rect(control_rect.intersect(ui.clip_rect()));
    add_control(&mut control_ui);

    let y = ui.painter().round_to_pixel_center(rect.bottom());
    ui.painter().hline(
        rect.x_range(),
        y,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
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
    ui.label(
        egui::RichText::new(text.into())
            .color(settings_text_secondary(ui))
            .size(SETTINGS_TYPE.row_description),
    );
}

fn detail_block(ui: &mut egui::Ui, label: &str, value: impl Into<String>) {
    ui.add_space(7.0);
    ui.vertical(|ui| {
        ui.set_width(ui.available_width());
        ui.label(egui::RichText::new(label).size(SETTINGS_TYPE.row_title));
        ui.add_space(3.0);
        detail_text(ui, value, true);
    });
    ui.add_space(7.0);
    settings_hairline(ui);
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
        ui.visuals().panel_fill
    };
    let stroke_color = if *on {
        ui.visuals().selection.bg_fill
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke.color
    };
    let painter = ui.painter();
    painter.rect(
        rect,
        radius,
        bg,
        egui::Stroke::new(1.0, stroke_color),
        egui::StrokeKind::Inside,
    );
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
    Keyboard,
    Bolt,
    Lock,
    Link,
    Grid,
    Diamond,
    Square,
    Clock,
    Bell,
    SystemIndicator,
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
        Icon::Keyboard => {
            let b = egui::Rect::from_center_size(c, egui::vec2(sz, sz * 0.72));
            p.rect_stroke(b, 1.5, s, egui::StrokeKind::Inside);
            for row in 0..2 {
                for column in 0..4 {
                    let key = egui::Rect::from_min_size(
                        egui::pos2(
                            b.left() + 2.0 + column as f32 * (sz - 4.0) / 4.0,
                            b.top() + 2.0 + row as f32 * (b.height() - 4.0) / 2.0,
                        ),
                        egui::vec2((sz - 8.0) / 4.0, (b.height() - 8.0) / 2.0),
                    );
                    p.rect_filled(key, 0.5, col);
                }
            }
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
        Icon::SystemIndicator => {
            let outer = egui::Rect::from_center_size(c, egui::vec2(sz * 0.84, sz * 0.84));
            let inner = egui::Rect::from_center_size(c, egui::vec2(sz * 0.48, sz * 0.48));
            p.rect_stroke(outer, 0.0, s, egui::StrokeKind::Inside);
            p.rect_filled(inner, 0.0, col);
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
            p.circle_filled(c, r * 0.56, col);
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
    let segment_width = SETTINGS_DETAIL.segment_width;
    let total = segment_width * items.len() as f32;
    let border = settings_input_border(ui);
    let surface = ui.visuals().panel_fill;
    let accent = ui.visuals().selection.bg_fill;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, h), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, surface);
    let mut changed = false;
    let mut x = rect.left();
    for (i, (theme, icon, label)) in items.iter().enumerate() {
        let seg =
            egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(segment_width, h));
        let on = *sel == *theme;
        let response = ui.interact(
            seg,
            ui.id().with(("settings_segment", i)),
            egui::Sense::click(),
        );
        if on {
            ui.painter().rect_filled(seg, 0.0, settings_nav_active(ui));
        } else if response.hovered() {
            ui.painter()
                .rect_filled(seg, 0.0, ui.visuals().widgets.hovered.bg_fill);
        }
        let col = if on {
            accent
        } else {
            settings_text_secondary(ui)
        };
        let cy = seg.center().y;
        let text_width = ui
            .painter()
            .layout_no_wrap(label.clone(), font.clone(), col)
            .size()
            .x;
        let content_width = icon_sz + gap + text_width;
        let content_left = seg.center().x - content_width / 2.0;
        paint_icon(
            ui.painter(),
            egui::pos2(content_left + icon_sz / 2.0, cy),
            icon_sz,
            *icon,
            col,
        );
        ui.painter().text(
            egui::pos2(content_left + icon_sz + gap, cy),
            egui::Align2::LEFT_CENTER,
            label,
            font.clone(),
            col,
        );
        if i < items.len() - 1 {
            ui.painter()
                .vline(seg.right(), rect.y_range(), egui::Stroke::new(1.0, border));
        }
        if response.clicked() && !on {
            *sel = *theme;
            changed = true;
        }
        x += segment_width;
    }
    ui.painter().rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );
    changed
}

/// React의 `SettingsSelect`: 230×34, 좌측 라벨·우측 화살표를 갖는 공통 선택 버튼.
fn settings_select_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(SETTINGS_DETAIL.select_width, CONTROL_HEIGHT),
        egui::Sense::click(),
    );
    let background = if response.hovered() {
        ui.visuals().widgets.hovered.bg_fill
    } else {
        ui.visuals().panel_fill
    };
    ui.painter().rect(
        rect,
        0.0,
        background,
        egui::Stroke::new(1.0, settings_input_border(ui)),
        egui::StrokeKind::Inside,
    );
    let label_clip = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 12.0, rect.top()),
        egui::pos2(rect.right() - 30.0, rect.bottom()),
    )
    .intersect(ui.clip_rect());
    ui.painter().with_clip_rect(label_clip).text(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(CONTROL_TEXT_SIZE),
        ui.visuals().text_color(),
    );
    let arrow_x = rect.right() - 12.0;
    let cy = rect.center().y;
    let d = 3.5;
    ui.painter().add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(arrow_x - d, cy - d * 0.6),
            egui::pos2(arrow_x + d, cy - d * 0.6),
            egui::pos2(arrow_x, cy + d * 0.7),
        ],
        ui.visuals().weak_text_color(),
        egui::Stroke::NONE,
    ));
    response
}

/// 사용자 입력 스텝퍼 — [값] │ [−] │ [+], 경계선 박스. 반환: 변경 여부.
fn stepper(ui: &mut egui::Ui, value: &mut i64, step: i64, min: i64, max: i64, unit: &str) -> bool {
    let h = CONTROL_HEIGHT;
    let btn_w = 44.0;
    let val_w = 138.0;
    let total = val_w + btn_w * 2.0;
    let hair = settings_input_border(ui);
    let input = ui.visuals().panel_fill;
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
    let btn_w = 44.0;
    let val_w = 138.0;
    let total = val_w + btn_w * 2.0;
    let hair = settings_input_border(ui);
    let input = ui.visuals().panel_fill;
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
                    Icon::SystemIndicator,
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
            let current_label = crate::fonts::effective_ui_font_name(
                config.ui.ui_font.as_deref(),
                FONT_OPTIONS.as_slice(),
            );
            let resp = settings_select_button(ui, &current_label);
            egui::Popup::menu(&resp).show(|ui| {
                ui.set_min_width(SETTINGS_DETAIL.select_width);
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
            let locale_label = current_locale_label(&config.i18n.locale, catalog);
            let resp = settings_select_button(ui, &locale_label);
            egui::Popup::menu(&resp).show(|ui| {
                ui.set_min_width(SETTINGS_DETAIL.select_width);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ShortcutPageFilter {
    #[default]
    All,
    Group(ShortcutGroup),
}

#[derive(Clone, Default)]
struct ShortcutPageState {
    query: String,
    filter: ShortcutPageFilter,
    recording: Option<ShortcutAction>,
    capture_error: bool,
}

fn shortcuts_page(
    ui: &mut egui::Ui,
    config: &mut Config,
    changed: &mut bool,
    catalog: &i18n::Catalog,
) {
    let state_id = egui::Id::new("settings_shortcuts_page_state");
    let mut state = ui.ctx().data_mut(|data| {
        data.get_temp::<ShortcutPageState>(state_id)
            .unwrap_or_default()
    });

    // 녹화 중에는 Escape=취소, Backspace/Delete=비우기, 나머지 modifier chord=저장.
    if let Some(action) = state.recording {
        let events = ui.input(|input| input.events.clone());
        for event in &events {
            let egui::Event::Key {
                key,
                pressed: true,
                repeat: false,
                ..
            } = event
            else {
                continue;
            };
            match key {
                egui::Key::Escape => {
                    state.recording = None;
                    state.capture_error = false;
                    break;
                }
                egui::Key::Backspace | egui::Key::Delete => {
                    shortcuts::set_binding(&mut config.shortcuts, action, None);
                    *changed = true;
                    state.recording = None;
                    state.capture_error = false;
                    break;
                }
                _ => {
                    if let Some(binding) = shortcuts::captured_binding(event) {
                        shortcuts::set_binding(&mut config.shortcuts, action, Some(binding));
                        *changed = true;
                        state.recording = None;
                        state.capture_error = false;
                        break;
                    }
                    if !matches!(
                        key,
                        egui::Key::ShiftLeft
                            | egui::Key::ShiftRight
                            | egui::Key::ControlLeft
                            | egui::Key::ControlRight
                            | egui::Key::AltLeft
                            | egui::Key::AltRight
                            | egui::Key::SuperLeft
                            | egui::Key::SuperRight
                    ) {
                        state.capture_error = true;
                    }
                }
            }
        }
    }

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(catalog.t("settings.shortcuts", &[]))
                .strong()
                .size(SETTINGS_TYPE.page_title),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(catalog.t("shortcuts.reset_all", &[]))
                .on_hover_text(catalog.t("shortcuts.reset_all.hint", &[]))
                .clicked()
            {
                shortcuts::reset_all(&mut config.shortcuts);
                *changed = true;
                state.recording = None;
            }
        });
    });
    ui.add_space(9.0);
    hint_text(ui, catalog.t("shortcuts.help", &[]));
    ui.add_space(12.0);
    settings_hairline(ui);
    ui.add_space(14.0);

    ui.horizontal(|ui| {
        let search_width = (ui.available_width() - 390.0).max(220.0);
        ui.add_sized(
            [search_width, CONTROL_HEIGHT],
            egui::TextEdit::singleline(&mut state.query)
                .hint_text(catalog.t("shortcuts.search", &[]))
                .margin(egui::Margin::symmetric(10, 7)),
        );
        ui.add_space(8.0);
        shortcut_filter_button(
            ui,
            &mut state.filter,
            ShortcutPageFilter::All,
            &catalog.t("shortcuts.filter.all", &[]),
        );
        for group in ShortcutGroup::ALL {
            shortcut_filter_button(
                ui,
                &mut state.filter,
                ShortcutPageFilter::Group(group),
                &catalog.t(group.title_key(), &[]),
            );
        }
    });

    let conflicts = shortcuts::conflicts(&config.shortcuts);
    if !conflicts.is_empty() {
        ui.add_space(10.0);
        let color = ui.visuals().error_fg_color;
        egui::Frame::NONE
            .fill(color.gamma_multiply(0.12))
            .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.65)))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.colored_label(
                    color,
                    egui::RichText::new(catalog.t("shortcuts.conflict", &[]))
                        .size(SETTINGS_TYPE.row_description),
                );
            });
    }
    if state.capture_error {
        ui.add_space(8.0);
        ui.colored_label(
            ui.visuals().warn_fg_color,
            egui::RichText::new(catalog.t("shortcuts.modifier_required", &[]))
                .size(SETTINGS_TYPE.row_description),
        );
    }

    let query = state.query.trim().to_lowercase();
    let mut rendered = 0usize;
    for group in ShortcutGroup::ALL {
        let filter_matches = match state.filter {
            ShortcutPageFilter::All => true,
            ShortcutPageFilter::Group(selected) => selected == group,
        };
        if !filter_matches {
            continue;
        }
        let actions: Vec<_> = ShortcutAction::ALL
            .into_iter()
            .filter(|action| action.group() == group)
            .filter(|action| {
                query.is_empty()
                    || catalog
                        .t(action.title_key(), &[])
                        .to_lowercase()
                        .contains(&query)
                    || catalog
                        .t(action.description_key(), &[])
                        .to_lowercase()
                        .contains(&query)
            })
            .collect();
        if actions.is_empty() {
            continue;
        }
        section(ui, &catalog.t(group.title_key(), &[]));
        for action in actions {
            rendered += 1;
            let title = catalog.t(action.title_key(), &[]);
            let description = catalog.t(action.description_key(), &[]);
            let is_recording = state.recording == Some(action);
            let is_conflict = conflicts.contains(&action);
            let custom = config.shortcuts.bindings.contains_key(action.id())
                || config.shortcuts.disabled.contains(action.id());
            let binding = shortcuts::effective_binding(&config.shortcuts, action);
            row(ui, &title, Some(&description), |ui| {
                let reset = ui
                    .add_enabled(custom, egui::Button::new(catalog.t("shortcuts.reset", &[])))
                    .on_hover_text(catalog.t("shortcuts.reset.hint", &[]));
                if reset.clicked() {
                    shortcuts::reset_binding(&mut config.shortcuts, action);
                    *changed = true;
                    if is_recording {
                        state.recording = None;
                    }
                }
                if ui
                    .button(catalog.t("shortcuts.clear", &[]))
                    .on_hover_text(catalog.t("shortcuts.clear.hint", &[]))
                    .clicked()
                {
                    shortcuts::set_binding(&mut config.shortcuts, action, None);
                    *changed = true;
                    state.recording = None;
                }
                let label = if is_recording {
                    catalog.t("shortcuts.recording", &[])
                } else if let Some(binding) = binding {
                    ui.ctx().format_shortcut(&binding)
                } else {
                    catalog.t("shortcuts.unassigned", &[])
                };
                let text = if is_conflict {
                    egui::RichText::new(label).color(ui.visuals().error_fg_color)
                } else if is_recording {
                    egui::RichText::new(label).color(ui.visuals().selection.bg_fill)
                } else {
                    egui::RichText::new(label)
                };
                if ui
                    .add_sized([172.0, CONTROL_HEIGHT], egui::Button::new(text))
                    .on_hover_text(catalog.t("shortcuts.record.hint", &[]))
                    .clicked()
                {
                    state.recording = if is_recording { None } else { Some(action) };
                    state.capture_error = false;
                }
            });
        }
    }
    if rendered == 0 {
        ui.add_space(18.0);
        hint_text(ui, catalog.t("settings.search.no_results", &[]));
    }

    ui.ctx().data_mut(|data| data.insert_temp(state_id, state));
}

fn shortcut_filter_button(
    ui: &mut egui::Ui,
    current: &mut ShortcutPageFilter,
    candidate: ShortcutPageFilter,
    label: &str,
) {
    if ui.selectable_label(*current == candidate, label).clicked() {
        *current = candidate;
    }
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
                .size(SETTINGS_TYPE.row_description),
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
                        .size(SETTINGS_TYPE.row_description),
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
                        .size(SETTINGS_TYPE.row_description),
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
    use super::{SETTINGS_DETAIL, SETTINGS_TYPE, nav_matches, truncate_fingerprint};

    #[test]
    fn 모양_화면_타이포그래피는_참조_font_map을_고정한다() {
        assert_eq!(SETTINGS_TYPE.page_title, 15.0);
        assert_eq!(SETTINGS_TYPE.section_title, 14.0);
        assert_eq!(SETTINGS_TYPE.row_title, 14.0);
        assert_eq!(SETTINGS_TYPE.row_description, 13.0);
        assert_eq!(SETTINGS_TYPE.control, 14.0);
    }

    #[test]
    fn 모양_화면_컴포넌트_치수는_참조값을_고정한다() {
        assert_eq!(SETTINGS_DETAIL.pad_x, 26.0);
        assert_eq!(SETTINGS_DETAIL.pad_top, 20.0);
        assert_eq!(SETTINGS_DETAIL.pad_bottom, 40.0);
        assert_eq!(SETTINGS_DETAIL.row_height, 68.0);
        assert_eq!(SETTINGS_DETAIL.control_column, 360.0);
        assert_eq!(SETTINGS_DETAIL.column_gap, 20.0);
        assert_eq!(SETTINGS_DETAIL.control_height, 34.0);
        assert_eq!(SETTINGS_DETAIL.select_width, 230.0);
        assert_eq!(SETTINGS_DETAIL.segment_width, 104.0);
    }

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
