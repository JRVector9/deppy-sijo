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

/// 모바일 웹(PWA) 섹션이 App에 돌려주는 동작 (RemoteAction 관례 — 의도만 전달).
pub enum WebRemoteAction {
    None,
    /// 토글 on — 웹서버 기동 요청.
    Start,
    /// 토글 off — 웹서버 정지 요청.
    Stop,
    /// 페어링 토큰 재발급 — 기존 페어링 무효, 실행 중이면 새 토큰으로 재시작.
    RotateToken,
    /// ts.net 호스트명 자동 감지 요청 (tailscale status --json — App이 1회성 스레드로 실행).
    DetectHostname,
    /// serve 상태 재진단 요청 (O1 — tailscale serve status --json).
    CheckServe,
    /// `tailscale serve --bg <port>` 실행 요청 (O1 — 사용자 클릭에서만).
    ConfigureServe,
    /// tailnet Serve 승인 페이지를 브라우저로 연다 (O1 — CLI가 준 URL).
    OpenApproveUrl(String),
}

/// ts.net 호스트명 자동 감지 표시 상태 (App이 감지 스레드 결과를 매핑해 넘긴다).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsDetectView<'a> {
    /// 아직 시도 안 함 — 아무것도 표시하지 않는다.
    Idle,
    /// 감지 스레드 진행 중.
    Running,
    /// CLI를 찾지 못함 (미설치 또는 알 수 없는 경로).
    NoCli,
    /// CLI는 있지만 호스트명 없음 (미로그인/정지/MagicDNS off).
    NoHostname,
    /// 감지 성공 — 설정값과 다르면 참고용으로 표시한다.
    Found(&'a str),
}

/// 모바일 웹 섹션 렌더 상태 (App이 채워 넘긴다 — UI는 서버/keyring을 직접 만지지 않는다).
pub struct WebRemoteView<'a> {
    /// 웹서버 실행 여부 — 토글 상태의 진실 소스.
    pub running: bool,
    /// 실행 중이면 bind 주소("127.0.0.1:포트").
    pub addr: Option<String>,
    /// 접속 URL(페어링 토큰 포함 — 민감). QR 원본. 실행 중에만 Some.
    pub url: Option<String>,
    /// 시작 실패 등 표시할 에러.
    pub error: Option<&'a str>,
    /// ts.net 호스트명 자동 감지 상태.
    pub ts_detect: TsDetectView<'a>,
    /// serve 온보딩 상태 (O1).
    pub serve: ServeView<'a>,
}

/// serve 온보딩 표시 상태 (O1). 폰 접속의 마지막 관문 — 앱 웹서버는 127.0.0.1에만
/// bind하므로 `tailscale serve`가 HTTPS를 종단해 프록시해야 폰이 붙는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeView<'a> {
    /// 아직 진단 안 함(웹서버 꺼짐 등) — 표시 없음.
    Idle,
    /// 진단/설정 스레드 진행 중.
    Running,
    /// 이 포트로 프록시가 걸려 있다 — 폰 접속 준비 완료.
    Ready,
    /// serve가 다른 포트를 가리킨다 — 재설정 필요.
    WrongPort(u16),
    /// serve 미설정 — 설정 버튼.
    NotConfigured,
    /// tailnet에서 Serve 기능 미활성 — 관리 콘솔 1회 승인 필요.
    NotEnabled { approve_url: Option<&'a str> },
    /// CLI 없음/진단 불가 — 문서 안내로 폴백.
    Unknown,
}

/// 접속 URL QR 텍스처 캐시 — (원본 URL, 텍스처). URL이 바뀔 때만 재생성한다.
pub type WebQrCache = Option<(String, egui::TextureHandle)>;

/// 통합 설정 창의 좌측 네비 카테고리. 설정 5개는 이 파일이 인라인 렌더하고, 관리/모니터
/// 7개는 App이 `render_management` 콜백으로 각 패널의 contents()를 렌더한다 (2026-07-06 전체 통합).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Category {
    #[default]
    General,
    Language,
    Terminal,
    Shortcuts,
    Performance,
    RemoteTls,
    MobileWeb,
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
    /// 모바일 웹 섹션 동작 요청.
    pub web_action: WebRemoteAction,
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
    web: &WebRemoteView,
    web_reveal_url: &mut bool,
    web_qr: &mut WebQrCache,
    notif_unread: u32,
    search_query: &mut String,
    catalog: &i18n::Catalog,
    mut render_management: impl FnMut(&mut egui::Ui, Category),
) -> SettingsOutput {
    let mut changed = false;
    let mut remote_action = RemoteAction::None;
    let mut web_action = WebRemoteAction::None;

    // title_bar(false)라 기본 open 처리가 없다 — 닫힘이면 창 자체를 만들지 않는다.
    if !*open {
        return SettingsOutput {
            config_changed: false,
            remote_action,
            web_action,
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
                                Category::MobileWeb => mobile_web_page(
                                    ui,
                                    config,
                                    web,
                                    web_reveal_url,
                                    web_qr,
                                    &mut changed,
                                    &mut web_action,
                                    catalog,
                                ),
                                other => render_management(ui, other),
                            };

                            apply_component_style(ui);
                            if matches!(*category, Category::Environment) {
                                render_detail(ui);
                            } else {
                                egui::ScrollArea::vertical()
                                    // 카테고리마다 scroll state를 분리한다. 단축키처럼 긴
                                    // 화면을 내린 뒤 활동으로 이동해도 이전 offset을 이어받아
                                    // 첫 워크스페이스가 화면 밖에서 시작하지 않는다.
                                    .id_salt(("settings_detail_scroll", *category))
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
        web_action,
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
            | Category::MobileWeb
    )
}

fn rgb(r: u8, g: u8, b: u8) -> egui::Color32 {
    egui::Color32::from_rgb(r, g, b)
}

/// `design/.../egui.ts`의 토큰을 설정 창 local visuals에 매핑한다.
pub(crate) fn apply_settings_palette(ui: &mut egui::Ui) {
    let dark = ui.visuals().dark_mode;
    // 설정 창은 완전 무채색(S 0%)이었고 사이드바는 색이 있어, 화면을 옮겨 다닐 때
    // 색온도가 출렁였다(2026-08-06). 명도(L)는 전부 그대로 두고 색상축·채도만
    // designall에 맞춘다 — 역할별 채도는 designall 실측치를 따른다:
    //   다크  표면 27% · 본문 22% · 보조 14% · muted 10% · border 21%
    //   라이트 표면 20/15% · 본문 11% · 보조 8% · muted 7% · border 13.5%
    // L을 유지하므로 명암비는 그대로다(본문 on surface 10.47 -> 10.48).
    // border_focus는 포커스 링이라 액센트와 다른 의미이므로 유지한다.
    let (bg, surface, surface_hover, panel, border, border_focus, text, muted, accent, input) =
        if dark {
            (
                rgb(0x16, 0x19, 0x1e), // was #1a1a1a
                rgb(0x1f, 0x22, 0x29), // was #242424
                rgb(0x25, 0x2a, 0x33), // was #2c2c2c
                rgb(0x1b, 0x1e, 0x25), // was #202020
                rgb(0x34, 0x38, 0x40), // was #3a3a3a
                rgb(0x5a, 0x9f, 0xd4),
                rgb(0xd0, 0xd3, 0xd8),             // was #d4d4d4
                rgb(0x69, 0x6e, 0x79),             // was #717171
                crate::ui::designall::DARK.accent, // was #4da6c8
                rgb(0x16, 0x19, 0x1e),             // was #1a1a1a (bg와 같은 값 유지)
            )
        } else {
            (
                rgb(0xdb, 0xde, 0xe5), // was #e0e0e0
                rgb(0xee, 0xef, 0xf2), // was #f0f0f0
                rgb(0xe5, 0xe7, 0xeb), // was #e8e8e8
                rgb(0xf9, 0xfa, 0xfb), // was #fafafa
                rgb(0xbc, 0xc1, 0xcc), // was #c4c4c4
                rgb(0x3a, 0x88, 0xbf),
                rgb(0x17, 0x19, 0x1d),              // was #1a1a1a
                rgb(0x80, 0x85, 0x90),              // was #888888
                crate::ui::designall::LIGHT.accent, // was #3a88bf
                rgb(0xff, 0xff, 0xff),              // input은 순백 유지
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
                (
                    Category::MobileWeb,
                    Icon::Phone,
                    catalog.t("settings.mobile_web", &[]),
                    "mobile web pwa phone qr pairing tailscale 모바일 웹 페어링",
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

// 아래 세 헬퍼도 apply_settings_palette와 같은 축(hsl 220도)을 쓴다. 명도는 유지.
// credentials.rs가 이 값을 재사용하므로 pub(crate)로 노출한다.
pub(crate) fn settings_text_secondary(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        rgb(0xa4, 0xa8, 0xb0) // was #aaaaaa
    } else {
        rgb(0x3f, 0x42, 0x49) // was #444444
    }
}

fn settings_input_border(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        rgb(0x3a, 0x3e, 0x46) // was #404040
    } else {
        rgb(0xae, 0xb5, 0xc2) // was #b8b8b8
    }
}

/// 선택 배경은 액센트가 섞인 표면이라 채도를 낮추면 "선택됨"이 약해진다 — 채도·명도는
/// 그대로 두고 색상축만 220도로 정렬했다.
/// env_project_list.rs가 이 값을 재사용하므로 pub(crate)로 노출한다.
pub(crate) fn settings_nav_active(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        rgb(0x2e, 0x3e, 0x5e) // was #2e4a5e (205도)
    } else {
        rgb(0xcc, 0xd7, 0xed) // was #ccdeed (207도)
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
    Phone,
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
        Icon::Phone => {
            // 스마트폰: 세로 라운드 사각 + 하단 홈 바
            let body = egui::Rect::from_center_size(c, egui::vec2(sz * 0.6, sz));
            p.rect_stroke(body, 2.5, s, egui::StrokeKind::Inside);
            p.line_segment(
                [
                    egui::pos2(c.x - r * 0.18, body.bottom() - r * 0.28),
                    egui::pos2(c.x + r * 0.18, body.bottom() - r * 0.28),
                ],
                s,
            );
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
        &catalog.t("settings.ui_scale", &[]),
        Some(&catalog.t("settings.ui_scale.hint", &[])),
        |ui| {
            let mut v = config.ui.ui_scale;
            // 스텝 0.1 + 0.1 격자 스냅 — 1.0→1.1처럼 한 번에 0.1씩, 부동소수 드리프트 방지.
            if stepper_f32(ui, &mut v, 0.1, 0.7, 1.5) {
                config.ui.ui_scale = (v * 10.0).round() / 10.0;
                *changed = true;
            }
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
        &catalog.t("settings.confirm_workspace_close", &[]),
        Some(&catalog.t("settings.confirm_workspace_close.hint", &[])),
        |ui| {
            if toggle_switch(ui, &mut config.ui.confirm_workspace_close) {
                *changed = true;
            }
        },
    );
    row(
        ui,
        &catalog.t("settings.session_name_style", &[]),
        Some(&catalog.t("settings.session_name_style.hint", &[])),
        |ui| {
            use crate::config::SessionNameStyle;
            let current_key = match config.ui.session_name_style {
                SessionNameStyle::Folder => "settings.session_name_style.folder",
                SessionNameStyle::Repo => "settings.session_name_style.repo",
            };
            let resp = settings_select_button(ui, &catalog.t(current_key, &[]));
            egui::Popup::menu(&resp).show(|ui| {
                ui.set_min_width(SETTINGS_DETAIL.select_width);
                for (style, key) in [
                    (
                        SessionNameStyle::Folder,
                        "settings.session_name_style.folder",
                    ),
                    (SessionNameStyle::Repo, "settings.session_name_style.repo"),
                ] {
                    let selected = config.ui.session_name_style == style;
                    if ui.selectable_label(selected, catalog.t(key, &[])).clicked() {
                        config.ui.session_name_style = style;
                        *changed = true;
                    }
                }
            });
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
    // (「메모리 표시」 토글 제거 — 2026-07-18 사용자: 메모리는 하단 상태바로 일원화,
    // 상단 옵트인 표시와 그 토글은 수치 불일치의 원천이라 삭제.)
    // 하단 도크 컴포저 표시 (2026-07-17 사용자) — OFF면 도크 패널 미생성(터미널이
    // 공간 회수) + FocusComposer 단축키 무시.
    row(
        ui,
        &catalog.t("settings.composer_enabled", &[]),
        Some(&catalog.t("settings.composer_enabled.hint", &[])),
        |ui| {
            if toggle_switch(ui, &mut config.ui.composer_enabled) {
                *changed = true;
            }
        },
    );
    // 컴포저 전송 키 (2026-07-17) — 개행 키는 자동 보완(Enter 전송이면 Shift+Enter=개행,
    // ⌘/Ctrl+Enter 전송이면 Enter=개행).
    row(
        ui,
        &catalog.t("settings.composer_send_key", &[]),
        Some(&catalog.t("settings.composer_send_key.hint", &[])),
        |ui| {
            use crate::config::ComposerSendKey;
            for (value, key) in [
                (ComposerSendKey::Enter, "settings.composer_send_key.enter"),
                (
                    ComposerSendKey::CmdEnter,
                    "settings.composer_send_key.cmd_enter",
                ),
                (
                    ComposerSendKey::CtrlEnter,
                    "settings.composer_send_key.ctrl_enter",
                ),
            ] {
                if ui
                    .radio_value(&mut config.ui.composer_send_key, value, catalog.t(key, &[]))
                    .changed()
                {
                    *changed = true;
                }
            }
        },
    );
    // 프롬프트 라이브러리 표시 (기능2 PR-7) — OFF면 컴포저 "/prompt" 버튼·팔레트를 숨긴다
    // (저장된 프롬프트 데이터는 보존).
    row(
        ui,
        &catalog.t("settings.prompt_library", &[]),
        Some(&catalog.t("settings.prompt_library.hint", &[])),
        |ui| {
            if toggle_switch(ui, &mut config.ui.prompt_library_enabled) {
                *changed = true;
            }
        },
    );
    // fleet 배치 스폰 한 번에 시작할 수 있는 최대 세션 수(PR-S1). max_live_warm 스타일의
    // stepper — 위젯 범위(1~16)는 config::normalize의 클램프와 동일하다.
    row(
        ui,
        &catalog.t("settings.fleet_batch_spawn_max", &[]),
        Some(&catalog.t("settings.fleet_batch_spawn_max.hint", &[])),
        |ui| {
            let mut v = config.ui.fleet_batch_spawn_max as i64;
            if stepper(ui, &mut v, 1, 1, 16, "") {
                config.ui.fleet_batch_spawn_max = v as u32;
                *changed = true;
            }
        },
    );
    agent_send_presets_section(ui, config, changed, catalog);
}

/// 「에이전트로 보내기」 프리셋 편집 (2026-07-17 사용자) — 터미널 선택 → 우클릭 메뉴에
/// 뜨는 문구들. 지금까지 config.toml을 직접 고쳐야 했다.
///
/// 목록 편집이라 `row`(라벨 + 위젯 1개) 틀에 안 맞아 전용 섹션으로 둔다. 저장은 다른
/// 설정과 같은 규약(`*changed = true` → 호출측이 config.save).
fn agent_send_presets_section(
    ui: &mut egui::Ui,
    config: &mut Config,
    changed: &mut bool,
    catalog: &i18n::Catalog,
) {
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(catalog.t("settings.agent_presets", &[]))
            .strong()
            .size(13.0),
    );
    ui.label(
        egui::RichText::new(catalog.t("settings.agent_presets.hint", &[]))
            .size(11.0)
            .weak(),
    );
    ui.add_space(4.0);

    let mut remove_at = None;
    for (i, preset) in config.ui.agent_send_presets.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            // 항목이 지워지거나 순서가 바뀌면 위치 기반 auto-Id가 밀려 포커스가 옆 칸으로
            // 튄다 — 인덱스로 고정한다(인박스 입력칸에서 같은 문제를 겪었다).
            let resp = ui.add(
                egui::TextEdit::singleline(preset)
                    .id_salt(("agent_preset", i))
                    .desired_width(ui.available_width() - 40.0),
            );
            if resp.changed() {
                *changed = true;
            }
            if ui
                .button("✕")
                .on_hover_text(catalog.t("settings.agent_presets.remove", &[]))
                .clicked()
            {
                remove_at = Some(i);
            }
        });
    }
    if let Some(i) = remove_at {
        config.ui.agent_send_presets.remove(i);
        *changed = true;
    }
    if ui
        .button(catalog.t("settings.agent_presets.add", &[]))
        .clicked()
    {
        config.ui.agent_send_presets.push(String::new());
        *changed = true;
    }
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
        &catalog.t("settings.line_height", &[]),
        Some(&catalog.t("settings.line_height.hint", &[])),
        |ui| {
            let mut v = config.terminal.line_height;
            // ui_scale과 동일 — 스텝 0.1(10%) + 0.1 격자 스냅으로 부동소수 드리프트 방지.
            if stepper_f32(ui, &mut v, 0.1, 0.8, 2.0) {
                config.terminal.line_height = (v * 10.0).round() / 10.0;
                *changed = true;
            }
        },
    );
    row(
        ui,
        &catalog.t("settings.mono_font", &[]),
        Some(&catalog.t("settings.mono_font.hint", &[])),
        |ui| {
            for f in crate::fonts::MONO_FONTS {
                if ui
                    .selectable_label(config.terminal.mono_font == *f, *f)
                    .clicked()
                {
                    config.terminal.mono_font = (*f).to_owned();
                    // 가족 전환 시 미지원 굵기는 Regular로 — D2Coding은 2단뿐.
                    if !crate::fonts::mono_weights_for(f)
                        .contains(&config.terminal.mono_weight.as_str())
                    {
                        config.terminal.mono_weight = crate::fonts::DEFAULT_MONO_WEIGHT.to_owned();
                    }
                    *changed = true;
                }
            }
        },
    );
    row(
        ui,
        &catalog.t("settings.mono_weight", &[]),
        Some(&catalog.t("settings.mono_weight.hint", &[])),
        |ui| {
            for w in crate::fonts::mono_weights_for(&config.terminal.mono_font) {
                if ui
                    .selectable_label(config.terminal.mono_weight == *w, *w)
                    .clicked()
                {
                    config.terminal.mono_weight = (*w).to_owned();
                    *changed = true;
                }
            }
        },
    );
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
    row(
        ui,
        &catalog.t("settings.exited_cap", &[]),
        Some(&catalog.t("settings.exited_cap.hint", &[])),
        |ui| {
            let mut v = config.terminal.exited_backend_cap as i64;
            if stepper(ui, &mut v, 4, 4, 512, "") {
                config.terminal.exited_backend_cap = v as u32;
                *changed = true;
            }
        },
    );
    row(
        ui,
        &catalog.t("settings.cache_budget", &[]),
        Some(&catalog.t("settings.cache_budget.hint", &[])),
        |ui| {
            let mut v = config.terminal.cache_budget_mb as i64;
            if stepper(ui, &mut v, 32, 32, 2_048, "MB") {
                config.terminal.cache_budget_mb = v as u32;
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

/// OS/터미널이 직접 처리해 앱이 재바인딩할 수 없는 고정 단축키 — Terminal 그룹 목록에
/// 표기만 하고 레코딩·초기화·해제 컨트롤은 제공하지 않는다. (title_key, key_label)
/// (맨 아래로 스크롤은 재바인딩 가능한 ShortcutAction::ScrollToBottom — 여기 아님.)
const FIXED_TERMINAL_SHORTCUTS: &[(&str, &str)] = &[
    ("shortcuts.action.copy", "⌘C"),
    ("shortcuts.action.paste", "⌘V"),
];

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

    // 녹화 중에는 Escape=취소, Backspace/Delete=비우기, 안전한 chord=저장.
    // 일반 문자는 modifier가 필요하지만 외부 키패드용 F13~F24는 단독 등록할 수 있다.
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
        // OS/터미널이 직접 처리하는 고정 단축키 — Terminal 그룹에만 표기용으로 덧붙인다.
        let fixed_shortcuts: Vec<(&str, &str)> = if group == ShortcutGroup::Terminal {
            FIXED_TERMINAL_SHORTCUTS
                .iter()
                .copied()
                .filter(|entry| {
                    query.is_empty() || catalog.t(entry.0, &[]).to_lowercase().contains(&query)
                })
                .collect()
        } else {
            Vec::new()
        };
        if actions.is_empty() && fixed_shortcuts.is_empty() {
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
        for (title_key, key_label) in fixed_shortcuts {
            rendered += 1;
            row(
                ui,
                &catalog.t(title_key, &[]),
                Some(&catalog.t("shortcuts.desc.terminal", &[])),
                |ui| {
                    ui.add_enabled(
                        false,
                        egui::Button::new(key_label).min_size(egui::vec2(172.0, CONTROL_HEIGHT)),
                    );
                },
            );
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
    // live warm hard cap — 이 기기 RAM 유도 권장값을 힌트에 함께 보인다.
    let recommended = crate::config::recommended_max_live_warm();
    row(
        ui,
        &catalog.t("settings.max_live_warm", &[]),
        Some(&catalog.t(
            "settings.max_live_warm.hint",
            &[("recommended", &recommended.to_string())],
        )),
        |ui| {
            let mut v = config.performance.max_live_warm as i64;
            if stepper(ui, &mut v, 1, 1, 12, "") {
                config.performance.max_live_warm = v as u32;
                *changed = true;
            }
        },
    );
    row(
        ui,
        &catalog.t("settings.max_warm", &[]),
        Some(&catalog.t("settings.max_warm.hint", &[])),
        |ui| {
            let mut v = config.performance.max_warm as i64;
            if stepper(ui, &mut v, 1, 0, 8, "") {
                config.performance.max_warm = v as u32;
                *changed = true;
            }
        },
    );
    row(
        ui,
        &catalog.t("settings.max_cross_workspace_panes", &[]),
        Some(&catalog.t("settings.max_cross_workspace_panes.hint", &[])),
        |ui| {
            let mut v = config.performance.max_cross_workspace_panes as i64;
            if stepper(ui, &mut v, 1, 1, 6, "") {
                config.performance.max_cross_workspace_panes = v as u32;
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

/// 모바일 웹(PWA) 페이지 (mobile-pwa 계획 v3.3 P1) — 토글/포트/호스트명 + 실행 중이면
/// 접속 URL·QR·토큰 재발급. cert 모드(자체 TLS)는 후속이라 안내 문구만 둔다.
#[allow(clippy::too_many_arguments)]
fn mobile_web_page(
    ui: &mut egui::Ui,
    config: &mut Config,
    web: &WebRemoteView,
    reveal_url: &mut bool,
    qr_cache: &mut WebQrCache,
    changed: &mut bool,
    web_action: &mut WebRemoteAction,
    catalog: &i18n::Catalog,
) {
    page_title(ui, &catalog.t("settings.mobile_web", &[]));
    // 토글 = 실행 중 OR 저장된 자동시작 의도 (remote_page와 동일 규칙).
    let mut enabled = web.running || config.web.enabled;
    row(
        ui,
        &catalog.t("settings.mobile_web_enabled", &[]),
        Some(&catalog.t("settings.mobile_web_enabled.hint", &[])),
        |ui| {
            if toggle_switch(ui, &mut enabled) {
                *web_action = if enabled {
                    WebRemoteAction::Start
                } else {
                    WebRemoteAction::Stop
                };
            }
        },
    );
    row(
        ui,
        &catalog.t("settings.port", &[]),
        Some(&catalog.t("settings.toggle_restart_required", &[])),
        |ui| {
            *changed |= ui
                .add(egui::DragValue::new(&mut config.web.port).range(0..=65535))
                .changed();
        },
    );
    row(
        ui,
        &catalog.t("settings.mobile_web.hostname", &[]),
        Some(&catalog.t("settings.mobile_web.hostname.hint", &[])),
        |ui| {
            // right_to_left 배치 — 버튼이 오른쪽 끝, 입력 필드가 남은 폭을 채운다.
            let detecting = web.ts_detect == TsDetectView::Running;
            let label = if detecting {
                catalog.t("settings.mobile_web.detecting", &[])
            } else {
                catalog.t("settings.mobile_web.detect", &[])
            };
            if ui
                .add_enabled(!detecting, egui::Button::new(label))
                .clicked()
            {
                *web_action = WebRemoteAction::DetectHostname;
            }
            *changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut config.web.ts_hostname)
                        .hint_text("machine.tailnet.ts.net")
                        .desired_width(ui.available_width()),
                )
                .changed();
        },
    );
    // 감지 결과 안내 — 실패는 원인별로, 성공은 설정값과 다를 때만 참고 표시.
    match web.ts_detect {
        TsDetectView::NoCli => {
            hint_text(ui, catalog.t("settings.mobile_web.detect_no_cli", &[]));
        }
        TsDetectView::NoHostname => {
            hint_text(ui, catalog.t("settings.mobile_web.detect_no_hostname", &[]));
        }
        TsDetectView::Found(host) if host != config.web.ts_hostname.trim() => {
            hint_text(
                ui,
                catalog.t("settings.mobile_web.detected", &[("hostname", host)]),
            );
        }
        _ => {}
    }
    if let Some(err) = web.error {
        ui.add_space(7.0);
        ui.colored_label(
            ui.visuals().error_fg_color,
            egui::RichText::new(catalog.t("settings.start_failed", &[("message", err)]))
                .size(SETTINGS_TYPE.row_description),
        );
        ui.add_space(7.0);
        settings_hairline(ui);
    }
    if web.running {
        if let Some(addr) = &web.addr {
            row(ui, &catalog.t("settings.address", &[]), None, |ui| {
                detail_text(ui, addr.as_str(), true);
            });
            // serve 온보딩 (O1) — 앱은 127.0.0.1에만 bind하므로 tailscale serve가 HTTPS를
            // 종단해야 폰이 붙는다. 상태별로 **다음 한 걸음만** 보여준다.
            let port = addr.rsplit_once(':').map(|(_, p)| p).unwrap_or("");
            serve_row(ui, web, port, web_action, catalog);
        }
        if let Some(url) = &web.url {
            // 접속 URL — 페어링 토큰이 실리므로 기본 마스킹. 복사는 항상 전체 URL.
            row(ui, &catalog.t("settings.mobile_web.url", &[]), None, |ui| {
                ui.checkbox(reveal_url, catalog.t("settings.show", &[]));
                if ui.button(catalog.t("action.copy", &[])).clicked() {
                    ui.ctx().copy_text(url.clone());
                }
            });
            let display = if *reveal_url {
                url.clone()
            } else {
                masked_url(url)
            };
            detail_text(ui, display, true);
            ui.colored_label(
                ui.visuals().warn_fg_color,
                egui::RichText::new(catalog.t("settings.mobile_web.url_warning", &[]))
                    .size(SETTINGS_TYPE.row_description),
            );
            // 페어링 토큰 재발급 — keyring의 토큰을 교체하고 실행 중이면 재시작.
            row(
                ui,
                &catalog.t("settings.token", &[]),
                Some(&catalog.t("settings.mobile_web.rotate.hint", &[])),
                |ui| {
                    if ui
                        .button(catalog.t("settings.mobile_web.rotate", &[]))
                        .clicked()
                    {
                        *web_action = WebRemoteAction::RotateToken;
                    }
                },
            );
            // 페어링 QR — 폰 카메라 스캔용 (URL 전체 = 토큰 포함).
            ui.add_space(14.0);
            show_qr(ui, qr_cache, url);
            ui.add_space(6.0);
            hint_text(ui, catalog.t("settings.mobile_web.qr_hint", &[]));
            if config.web.ts_hostname.trim().is_empty() {
                hint_text(ui, catalog.t("settings.mobile_web.qr_needs_hostname", &[]));
            }
            ui.add_space(7.0);
            settings_hairline(ui);
        }
    }
    ui.add_space(14.0);
    // cert 모드(자체 TLS + 비-loopback bind)는 후속 — config 키만 예약돼 있다.
    hint_text(ui, catalog.t("settings.mobile_web.cert_note", &[]));
}

/// serve 온보딩 행 (O1) — 진단 상태 + 다음 한 걸음 버튼.
fn serve_row(
    ui: &mut egui::Ui,
    web: &WebRemoteView,
    port: &str,
    web_action: &mut WebRemoteAction,
    catalog: &i18n::Catalog,
) {
    ui.add_space(7.0);
    let running = web.serve == ServeView::Running;
    match &web.serve {
        // 진단 전/불가 — 기존 문서 안내(수동 명령)로 폴백한다. 하드 실패 금지.
        ServeView::Idle | ServeView::Unknown => {
            hint_text(
                ui,
                catalog.t("settings.mobile_web.serve_guide", &[("port", port)]),
            );
        }
        ServeView::Running => {
            hint_text(ui, catalog.t("settings.mobile_web.serve_checking", &[]));
        }
        ServeView::Ready => {
            ui.colored_label(
                // 완료 톤은 agent_visuals가 소유한다 — 일회성 #58b368을 쓰면 같은 의미가
                // 화면마다 다른 초록이 된다(2026-08-06).
                crate::ui::agent_visuals::status_color(
                    crate::agent_surface::AgentVisualState::Complete,
                ),
                egui::RichText::new(catalog.t("settings.mobile_web.serve_ready", &[]))
                    .size(SETTINGS_TYPE.row_description),
            );
        }
        ServeView::NotConfigured | ServeView::WrongPort(_) => {
            let message = match &web.serve {
                ServeView::WrongPort(other) => catalog.t(
                    "settings.mobile_web.serve_wrong_port",
                    &[("other", &other.to_string()), ("port", port)],
                ),
                _ => catalog.t("settings.mobile_web.serve_missing", &[]),
            };
            ui.colored_label(
                ui.visuals().warn_fg_color,
                egui::RichText::new(message).size(SETTINGS_TYPE.row_description),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !running,
                        egui::Button::new(catalog.t("settings.mobile_web.serve_setup", &[])),
                    )
                    .clicked()
                {
                    *web_action = WebRemoteAction::ConfigureServe;
                }
                if ui
                    .add_enabled(
                        !running,
                        egui::Button::new(catalog.t("settings.mobile_web.serve_recheck", &[])),
                    )
                    .clicked()
                {
                    *web_action = WebRemoteAction::CheckServe;
                }
            });
        }
        // tailnet 관리 콘솔에서 1회 승인이 필요하다 — 앱이 대신할 수 없는 유일한 단계.
        ServeView::NotEnabled { approve_url } => {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                egui::RichText::new(catalog.t("settings.mobile_web.serve_not_enabled", &[]))
                    .size(SETTINGS_TYPE.row_description),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if let Some(url) = approve_url
                    && ui
                        .button(catalog.t("settings.mobile_web.serve_approve", &[]))
                        .clicked()
                {
                    *web_action = WebRemoteAction::OpenApproveUrl((*url).to_owned());
                }
                if ui
                    .add_enabled(
                        !running,
                        egui::Button::new(catalog.t("settings.mobile_web.serve_recheck", &[])),
                    )
                    .clicked()
                {
                    *web_action = WebRemoteAction::CheckServe;
                }
            });
        }
    }
    ui.add_space(7.0);
    settings_hairline(ui);
}

/// 접속 URL의 token 값 부분을 마스킹한다 (표시 전용 — 복사/QR는 전체를 쓴다).
fn masked_url(url: &str) -> String {
    match url.split_once("token=") {
        Some((head, _)) => format!("{head}token=…"),
        None => url.to_owned(),
    }
}

/// 접속 URL QR를 그린다. 텍스처는 1px/모듈로 만들고 NEAREST 정수 배율로 확대해
/// 모듈 경계가 뭉개지지 않게 한다. URL이 바뀔 때만 재생성.
fn show_qr(ui: &mut egui::Ui, cache: &mut WebQrCache, url: &str) {
    if cache
        .as_ref()
        .is_none_or(|(cached_url, _)| cached_url != url)
    {
        let Some(image) = qr_color_image(url) else {
            hint_text(ui, "QR 생성 실패");
            return;
        };
        let texture = ui
            .ctx()
            .load_texture("web_remote_qr", image, egui::TextureOptions::NEAREST);
        *cache = Some((url.to_owned(), texture));
    }
    if let Some((_, texture)) = cache {
        let size = texture.size_vec2();
        let scale = (200.0 / size.x).floor().max(1.0);
        ui.add(egui::Image::new((texture.id(), size * scale)));
    }
}

/// URL을 QR 매트릭스로 인코드해 흑백 이미지로 만든다 (quiet zone 4모듈 포함 — 스캐너 요구).
fn qr_color_image(data: &str) -> Option<egui::ColorImage> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let margin = 4usize;
    let size = width + margin * 2;
    let mut image = egui::ColorImage::filled([size, size], egui::Color32::WHITE);
    for y in 0..width {
        for x in 0..width {
            if colors[y * width + x] == qrcode::Color::Dark {
                image.pixels[(y + margin) * size + (x + margin)] = egui::Color32::BLACK;
            }
        }
    }
    Some(image)
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
    use super::{
        SETTINGS_DETAIL, SETTINGS_TYPE, masked_url, nav_matches, qr_color_image,
        truncate_fingerprint,
    };

    /// egui `check_for_id_clash`가 그리는 "🔥 … use of … ID" 경고 텍스트 수집
    /// (env_profiles.rs 테스트의 동명 헬퍼와 같은 판정 — 그쪽 발화 테스트가 이 판정이
    /// 실제 충돌을 잡는다는 것을 함께 고정한다).
    fn clash_warning_texts(output: &egui::FullOutput) -> Vec<(String, egui::Pos2)> {
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => {
                    let s = text.galley.text();
                    if s.contains("use of") {
                        Some((s.to_owned(), text.pos))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect()
    }

    /// 회귀 고정(2026-07-18 "설정 화면 빨간 경고" 보고 후속): 인라인 설정 페이지
    /// 7종 × 로케일 2종을 네비와 함께 `warn_on_id_clash`를 켠 채 렌더해도 같은-ID
    /// 위젯 쌍 경고(🔥)가 없어야 한다. (당시 보고의 실제 충돌은 관리 카테고리인
    /// 환경 변수 표 — env_profiles.rs 테스트 참조.)
    #[test]
    fn kittest_설정_인라인_페이지에_widget_id_충돌이_없다() {
        use super::{Category, RemoteAction, RemoteView, WebRemoteAction, WebRemoteView};
        use crate::config::Config;
        for locale in [i18n::FALLBACK_LOCALE, "ko-KR"] {
            let catalog = i18n::Catalog::load(locale).unwrap();
            for category in [
                Category::General,
                Category::Language,
                Category::Terminal,
                Category::Shortcuts,
                Category::Performance,
                Category::RemoteTls,
                Category::MobileWeb,
            ] {
                let mut config = Config::default();
                config.ui.agent_send_presets = vec!["preset-a".to_owned(), "preset-b".to_owned()];
                let catalog_ref = &catalog;
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(egui::vec2(1280.0, 730.0))
                    .build_ui_state(
                        move |ui, state: &mut (Config, Category, String)| {
                            let (config, cat, query) = state;
                            let known_hosts =
                                vec![("host-a".to_owned(), "AA:BB:CC:DD:EE:FF".to_owned())];
                            let remote = RemoteView {
                                running: true,
                                addr: Some("127.0.0.1:7070".to_owned()),
                                fingerprint: Some("AB:CD"),
                                token: Some("secret-token"),
                                error: Some("boom"),
                                known_hosts_path: "/tmp/kh".to_owned(),
                                known_hosts: &known_hosts,
                            };
                            let web = WebRemoteView {
                                running: true,
                                addr: Some("127.0.0.1:8080".to_owned()),
                                url: Some("https://host.ts.net/?token=tok".to_owned()),
                                error: Some("boom"),
                                ts_detect: super::TsDetectView::Found("machine.ts.net"),
                                serve: super::ServeView::NotConfigured,
                            };
                            let mut changed = false;
                            let mut remote_action = RemoteAction::None;
                            let mut web_action = WebRemoteAction::None;
                            let mut reveal = true;
                            let mut qr: super::WebQrCache = None;
                            super::apply_settings_palette(ui);
                            egui::Panel::left("settings_nav")
                                .resizable(false)
                                .exact_size(190.0)
                                .show(ui, |ui| {
                                    super::nav(ui, cat, 3, query, catalog_ref);
                                });
                            egui::CentralPanel::default().show(ui, |ui| {
                                super::apply_component_style(ui);
                                egui::ScrollArea::vertical()
                                    .id_salt(("settings_detail_scroll", *cat))
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        super::apply_component_style(ui);
                                        super::settings_detail_shell(ui, |ui| match *cat {
                                            Category::General => super::general_page(
                                                ui,
                                                config,
                                                &mut changed,
                                                catalog_ref,
                                            ),
                                            Category::Language => super::language_page(
                                                ui,
                                                config,
                                                &mut changed,
                                                catalog_ref,
                                            ),
                                            Category::Terminal => super::terminal_page(
                                                ui,
                                                config,
                                                &mut changed,
                                                catalog_ref,
                                            ),
                                            Category::Shortcuts => super::shortcuts_page(
                                                ui,
                                                config,
                                                &mut changed,
                                                catalog_ref,
                                            ),
                                            Category::Performance => super::performance_page(
                                                ui,
                                                config,
                                                &mut changed,
                                                catalog_ref,
                                            ),
                                            Category::RemoteTls => super::remote_page(
                                                ui,
                                                config,
                                                &remote,
                                                &mut reveal,
                                                &mut changed,
                                                &mut remote_action,
                                                catalog_ref,
                                            ),
                                            Category::MobileWeb => super::mobile_web_page(
                                                ui,
                                                config,
                                                &web,
                                                &mut reveal,
                                                &mut qr,
                                                &mut changed,
                                                &mut web_action,
                                                catalog_ref,
                                            ),
                                            _ => {}
                                        });
                                    });
                            });
                        },
                        (config, category, String::new()),
                    );
                harness.ctx.options_mut(|o| o.warn_on_id_clash = true);
                harness.step();
                harness.step();
                harness.step();
                let warnings = clash_warning_texts(harness.output());
                assert!(
                    warnings.is_empty(),
                    "[{locale}/{category:?}] 위젯 ID 충돌 경고 발생: {warnings:?}"
                );
            }
        }
    }

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
    fn 접속_url_마스킹은_토큰만_가린다() {
        assert_eq!(
            masked_url("https://mac.ts.net/?token=abcd1234"),
            "https://mac.ts.net/?token=…"
        );
        // token 파라미터가 없으면 그대로
        assert_eq!(masked_url("https://mac.ts.net/"), "https://mac.ts.net/");
    }

    #[test]
    fn qr_이미지는_quiet_zone을_포함한_정방형() {
        let image = qr_color_image("https://mac.ts.net/?token=abc").unwrap();
        assert_eq!(image.size[0], image.size[1]);
        // 최소 QR(21모듈) + quiet zone 4×2
        assert!(image.size[0] >= 21 + 8, "{}", image.size[0]);
        // 흑백 두 색만
        assert!(
            image
                .pixels
                .iter()
                .all(|p| *p == egui::Color32::BLACK || *p == egui::Color32::WHITE)
        );
        // 테두리(quiet zone)는 흰색
        assert_eq!(image.pixels[0], egui::Color32::WHITE);
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
