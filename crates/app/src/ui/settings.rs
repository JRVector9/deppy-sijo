use crate::config::{Config, Theme};

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

    let win_frame = egui::Frame::window(&ctx.global_style()).inner_margin(egui::Margin::ZERO);
    egui::Window::new(catalog.t("settings.title", &[]))
        .title_bar(false) // 기본 타이틀바(높음) 제거 — 컴팩트 커스텀 행 사용 (목업 §설정)
        .frame(win_frame) // 여백 0 — 타이틀 라인이 창 끝까지(#6), 좌측 이중 여백 제거(#1)
        .collapsible(false)
        .default_pos([140.0, 90.0])
        .default_size([1000.0, 640.0])
        .min_size([720.0, 460.0])
        .show(ctx, |ui| {
            // 컴팩트 타이틀 행 (세로 30px) — 드래그 이동 + × 닫기
            // 타이틀 행: 드래그는 잡지 않는다 — StartDrag는 OS 창 전체를 움직여
            // 설정 창이 못 움직였다(#77). 빈 영역 드래그는 egui Window(Area) 이동.
            let (bar, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
            ui.painter().text(
                bar.center(),
                egui::Align2::CENTER_CENTER,
                catalog.t("settings.title", &[]),
                egui::FontId::proportional(13.5),
                ui.visuals().weak_text_color(),
            );
            let x_rect = egui::Rect::from_center_size(
                egui::pos2(bar.right() - 18.0, bar.center().y),
                egui::vec2(24.0, 24.0),
            );
            let xr = ui.interact(x_rect, ui.id().with("set_close"), egui::Sense::click());
            ui.painter().text(
                x_rect.center(),
                egui::Align2::CENTER_CENTER,
                "×",
                egui::FontId::proportional(16.0),
                if xr.hovered() {
                    ui.visuals().text_color()
                } else {
                    ui.visuals().weak_text_color()
                },
            );
            if xr.clicked() {
                *open = false;
            }
            ui.painter().hline(
                bar.x_range(),
                bar.bottom(),
                ui.visuals().widgets.noninteractive.bg_stroke,
            );

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
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        match *category {
                            Category::General => general_page(ui, config, &mut changed, catalog),
                            Category::Language => language_page(ui, config, &mut changed, catalog),
                            Category::Terminal => terminal_page(ui, config, &mut changed, catalog),
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
                            // 관리/모니터 7개 — App이 각 패널 contents() 렌더.
                            // 버튼·입력을 디자인 룰(docs/ui-components.md)로 통일한 뒤 렌더.
                            other => {
                                apply_component_style(ui);
                                render_management(ui, other)
                            }
                        }
                    });
            });
        });

    SettingsOutput {
        config_changed: changed,
        remote_action,
    }
}

/// 관리/모니터 패널의 버튼·입력을 디자인 룰로 통일한다 (#2·#3·#4, docs/ui-components.md).
/// 컨트롤 높이 30, 버튼 배경 accent-soft(#85 색)·라운딩 6, 텍스트 중앙(egui 버튼 기본).
fn apply_component_style(ui: &mut egui::Ui) {
    let accent = ui.visuals().selection.bg_fill;
    let soft = accent.gamma_multiply(0.16);
    let hover = accent.gamma_multiply(0.30);
    let radius = egui::CornerRadius::same(6);
    let spacing = ui.spacing_mut();
    spacing.interact_size.y = 30.0; // 버튼·입력·드롭다운 높이 통일
    spacing.button_padding = egui::vec2(12.0, 7.0);
    let v = ui.visuals_mut();
    v.widgets.inactive.weak_bg_fill = soft;
    v.widgets.inactive.bg_fill = soft;
    v.widgets.inactive.corner_radius = radius;
    v.widgets.hovered.weak_bg_fill = hover;
    v.widgets.hovered.bg_fill = hover;
    v.widgets.hovered.corner_radius = radius;
    v.widgets.active.weak_bg_fill = hover;
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
                ui.weak(catalog.t("settings.group.settings", &[]));
                for (cat, icon, label, _) in visible_settings {
                    rendered += 1;
                    nav_item(ui, category, cat, icon, &label, None);
                }
            }

            let manage = [
                (
                    Category::Credentials,
                    Icon::Key,
                    catalog.t("top.credentials", &[]),
                    "credentials secrets key api token password",
                ),
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
                    "environment env profile variables production",
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
                ui.weak(catalog.t("settings.group.manage", &[]));
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
                ui.weak(catalog.t("settings.group.monitor", &[]));
                for (cat, icon, label, _) in visible_monitor {
                    rendered += 1;
                    let item_badge = (cat == Category::Notifications)
                        .then(|| badge.clone())
                        .flatten();
                    nav_item(ui, category, cat, icon, &label, item_badge);
                }
            }

            if rendered == 0 {
                ui.weak(catalog.t("settings.search.no_results", &[]));
            }
        });
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
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());
    let accent = ui.visuals().selection.bg_fill;
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 7.0, accent.gamma_multiply(0.15));
    } else if resp.hovered() {
        p.rect_filled(rect, 7.0, ui.visuals().widgets.hovered.weak_bg_fill);
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
        egui::FontId::proportional(13.5),
        tc,
    );
    if let Some(b) = badge {
        let br = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 16.0, cy),
            egui::vec2(18.0, 16.0),
        );
        p.rect_filled(br, 8.0, egui::Color32::from_rgb(0xe7, 0x8a, 0x4e));
        p.text(
            br.center(),
            egui::Align2::CENTER_CENTER,
            b,
            egui::FontId::proportional(11.0),
            egui::Color32::from_rgb(0x1a, 0x1a, 0x1a),
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

/// 섹션 제목.
fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(10.0);
    ui.label(egui::RichText::new(title).size(16.0).strong());
    ui.add_space(4.0);
}

/// label(+hint) 왼쪽, 컨트롤 오른쪽. 아래 픽셀-스냅 헤어라인.
fn row(
    ui: &mut egui::Ui,
    label: &str,
    hint: Option<&str>,
    add_control: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(label).size(13.5));
            if let Some(h) = hint {
                ui.label(egui::RichText::new(h).weak().size(11.5));
            }
        });
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            add_control,
        );
    });
    ui.add_space(8.0);
    crate::ui::hairline(ui);
}

/// 토글 스위치 (checkbox 대체 — 목업 스타일). 값이 바뀌면 true.
fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(38.0, 22.0), egui::Sense::click());
    let mut changed = false;
    if resp.clicked() {
        *on = !*on;
        changed = true;
    }
    let radius = rect.height() / 2.0;
    let bg = if *on {
        ui.visuals().selection.bg_fill
    } else {
        ui.visuals().widgets.inactive.bg_fill
    };
    let painter = ui.painter();
    painter.rect_filled(rect, radius, bg);
    let t = if *on { 1.0 } else { 0.0 };
    let knob_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    painter.circle_filled(
        egui::pos2(knob_x, rect.center().y),
        radius - 3.0,
        egui::Color32::from_rgb(0xf4, 0xf4, 0xf6),
    );
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
    Key,
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
        Icon::Key => {
            p.circle_stroke(egui::pos2(c.x - r * 0.4, c.y - r * 0.4), r * 0.4, s);
            p.line_segment(
                [
                    egui::pos2(c.x - r * 0.15, c.y - r * 0.15),
                    egui::pos2(c.x + r * 0.7, c.y + r * 0.7),
                ],
                s,
            );
            p.line_segment(
                [
                    egui::pos2(c.x + r * 0.5, c.y + r * 0.5),
                    egui::pos2(c.x + r * 0.7, c.y + r * 0.3),
                ],
                s,
            );
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
    let font = egui::FontId::proportional(12.5);
    let h = 30.0;
    let icon_sz = 14.0;
    let gap = 6.0;
    let pad = 12.0;
    let hair = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let panel2 = ui.visuals().widgets.inactive.bg_fill;
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
        7.0,
        panel2,
        egui::Stroke::new(1.0, hair),
        egui::StrokeKind::Inside,
    );
    let mut changed = false;
    let mut x = rect.left();
    for (i, (theme, icon, label)) in items.iter().enumerate() {
        let seg = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(widths[i], h));
        let on = *sel == *theme;
        if on {
            ui.painter()
                .rect_filled(seg.shrink(2.0), 5.0, accent.gamma_multiply(0.15));
        }
        let col = if on {
            accent
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
    let h = 30.0;
    let btn_w = 30.0;
    let val_w = 70.0;
    let total = val_w + btn_w * 2.0;
    let hair = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let panel2 = ui.visuals().widgets.inactive.bg_fill;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, h), egui::Sense::hover());
    ui.painter().rect(
        rect,
        6.0,
        panel2,
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
        egui::FontId::monospace(13.0),
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
    let h = 30.0;
    let btn_w = 30.0;
    let val_w = 70.0;
    let total = val_w + btn_w * 2.0;
    let hair = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let panel2 = ui.visuals().widgets.inactive.bg_fill;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total, h), egui::Sense::hover());
    ui.painter().rect(
        rect,
        6.0,
        panel2,
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
        egui::FontId::monospace(13.0),
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
    section(ui, &catalog.t("settings.appearance", &[]));
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
    section(ui, &catalog.t("settings.language", &[]));
    row(
        ui,
        &catalog.t("settings.locale", &[]),
        Some(&catalog.t("settings.locale.hint", &[])),
        |ui| {
            // egui ComboBox는 selected_text를 좌측정렬(하드코딩)이라 텍스트 중앙정렬이 안 된다
            // → 커스텀 박스(중앙 텍스트 + ▾) + Popup::menu로 구현한다(#6).
            let w = 130.0;
            let h = 30.0;
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
            let hair = ui.visuals().widgets.noninteractive.bg_stroke.color;
            let bg = if resp.hovered() {
                ui.visuals().widgets.hovered.bg_fill
            } else {
                ui.visuals().widgets.inactive.bg_fill
            };
            ui.painter().rect(
                rect,
                6.0,
                bg,
                egui::Stroke::new(1.0, hair),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                current_locale_label(&config.i18n.locale, catalog),
                egui::FontId::proportional(13.0),
                ui.visuals().text_color(),
            );
            ui.painter().text(
                egui::pos2(rect.right() - 12.0, rect.center().y),
                egui::Align2::CENTER_CENTER,
                "▾",
                egui::FontId::proportional(11.0),
                ui.visuals().weak_text_color(),
            );
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
    section(ui, &catalog.t("settings.terminal", &[]));
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
    section(ui, &catalog.t("settings.performance", &[]));
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
    section(ui, &catalog.t("settings.remote_tls", &[]));
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
        ui.colored_label(
            ui.visuals().error_fg_color,
            catalog.t("settings.start_failed", &[("message", err)]),
        );
    }
    if remote.running {
        if let Some(addr) = &remote.addr {
            row(ui, &catalog.t("settings.address", &[]), None, |ui| {
                ui.add(egui::Label::new(egui::RichText::new(addr).monospace()).selectable(true));
            });
        }
        if let Some(fp) = remote.fingerprint {
            ui.add_space(6.0);
            ui.label(catalog.t("settings.fingerprint", &[]));
            ui.add(
                egui::Label::new(egui::RichText::new(fp).monospace())
                    .selectable(true)
                    .wrap(),
            );
            crate::ui::hairline(ui);
        }
        if let Some(token) = remote.token {
            row(ui, &catalog.t("settings.token", &[]), None, |ui| {
                ui.checkbox(reveal_token, catalog.t("settings.show", &[]));
            });
            if *reveal_token {
                ui.add(
                    egui::Label::new(egui::RichText::new(token).monospace())
                        .selectable(true)
                        .wrap(),
                );
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    catalog.t("settings.token_sensitive_warning", &[]),
                );
            } else {
                ui.weak(catalog.t("settings.token_hidden_hint", &[]));
            }
        }
        ui.weak(catalog.t("settings.client_fingerprint_hint", &[]));
    }

    section(ui, &catalog.t("settings.known_hosts", &[]));
    ui.weak(remote.known_hosts_path.as_str());
    if remote.known_hosts.is_empty() {
        ui.weak(catalog.t("settings.no_trust_records", &[]));
    } else {
        for (host, fp) in remote.known_hosts {
            ui.horizontal(|ui| {
                ui.monospace(host);
                ui.weak(truncate_fingerprint(fp, 17));
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
