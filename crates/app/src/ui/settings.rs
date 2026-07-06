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

/// 통합 설정 창의 좌측 네비 — 인라인으로 렌더하는 "설정" 카테고리.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Category {
    #[default]
    General,
    Language,
    Terminal,
    Performance,
    RemoteTls,
}

/// "관리"/"모니터" 그룹 — 아직 별도 패널을 여는 기존 기능들. 통합 창 네비에서 선택 시
/// App이 해당 패널을 연다 (전체 인라인화는 후속 — 2026-07-06).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OpenPanel {
    Credentials,
    Connectors,
    Environment,
    Agents,
    Workspaces,
    Activity,
    Notifications,
}

/// 설정 창 결과.
pub struct SettingsOutput {
    /// config 값이 바뀌어 저장이 필요한가 (테마/터미널/성능/포트).
    pub config_changed: bool,
    /// Remote 섹션 동작 요청.
    pub remote_action: RemoteAction,
    /// 관리/모니터 네비 항목 클릭 — App이 해당 패널을 연다.
    pub open_panel: Option<OpenPanel>,
}

/// 통합 설정 창 (2026-07-06 — 흩어진 툴바 기능을 좌측 네비 한 창으로).
/// 좌측: 검색 + 그룹별 카테고리. 우측: 선택된 카테고리의 폼. config 변경/Remote 동작/패널
/// 열기 요청을 [`SettingsOutput`]으로 돌려준다.
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
) -> SettingsOutput {
    let mut changed = false;
    let mut remote_action = RemoteAction::None;
    let mut open_panel = None;

    egui::Window::new(catalog.t("settings.title", &[]))
        .open(open)
        .collapsible(false)
        .default_size([1000.0, 640.0])
        .min_size([720.0, 460.0])
        .show(ctx, |ui| {
            egui::Panel::left("settings_nav")
                .resizable(false)
                .exact_size(216.0)
                .show(ui, |ui| {
                    nav(
                        ui,
                        category,
                        &mut open_panel,
                        notif_unread,
                        search_query,
                        catalog,
                    );
                });
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        match category {
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
                        }
                    });
            });
        });

    SettingsOutput {
        config_changed: changed,
        remote_action,
        open_panel,
    }
}

// ── 좌측 네비 ──

fn nav(
    ui: &mut egui::Ui,
    category: &mut Category,
    open_panel: &mut Option<OpenPanel>,
    notif_unread: u32,
    search_query: &mut String,
    catalog: &i18n::Catalog,
) {
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::singleline(search_query)
            .hint_text(catalog.t("settings.search", &[]))
            .desired_width(ui.available_width()),
    );
    ui.add_space(8.0);

    let query = search_query.trim();
    let mut rendered = 0usize;

    let settings = [
        (
            Category::General,
            "G",
            catalog.t("settings.cat.general", &[]),
            "general appearance theme folder tree sidebar ui",
        ),
        (
            Category::Language,
            "L",
            catalog.t("settings.language", &[]),
            "language locale i18n english japanese chinese korean",
        ),
        (
            Category::Terminal,
            "T",
            catalog.t("settings.terminal", &[]),
            "terminal font scrollback shell paste clipboard",
        ),
        (
            Category::Performance,
            "P",
            catalog.t("settings.performance", &[]),
            "performance output batch cpu memory rss resource",
        ),
        (
            Category::RemoteTls,
            "R",
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
            nav_item(ui, category, cat, icon, &label);
        }
    }

    let manage = [
        (
            OpenPanel::Credentials,
            "K",
            catalog.t("top.credentials", &[]),
            "credentials secrets key api token password",
        ),
        (
            OpenPanel::Connectors,
            "C",
            catalog.t("top.connectors", &[]),
            "connectors mcp tools oauth server",
        ),
        (
            OpenPanel::Environment,
            "E",
            catalog.t("top.environment", &[]),
            "environment env profile variables production",
        ),
        (
            OpenPanel::Agents,
            "A",
            catalog.t("top.agents", &[]),
            "agents command runner status regex",
        ),
        (
            OpenPanel::Workspaces,
            "W",
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
        for (panel, icon, label, _) in visible_manage {
            rendered += 1;
            nav_open(ui, icon, &label, None, open_panel, panel);
        }
    }

    let badge = (notif_unread > 0).then(|| notif_unread.to_string());
    let monitor = [
        (
            OpenPanel::Activity,
            "M",
            catalog.t("top.activity", &[]),
            "activity monitor cpu rss memory process workspace backpressure",
        ),
        (
            OpenPanel::Notifications,
            "N",
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
        for (panel, icon, label, _) in visible_monitor {
            rendered += 1;
            let item_badge = (panel == OpenPanel::Notifications)
                .then(|| badge.clone())
                .flatten();
            nav_open(ui, icon, &label, item_badge, open_panel, panel);
        }
    }

    if rendered == 0 {
        ui.weak(catalog.t("settings.search.no_results", &[]));
    }
}

fn nav_item(ui: &mut egui::Ui, current: &mut Category, cat: Category, icon: &str, label: &str) {
    let selected = *current == cat;
    let text = format!("{icon}  {label}");
    if ui.selectable_label(selected, text).clicked() {
        *current = cat;
    }
}

fn nav_open(
    ui: &mut egui::Ui,
    icon: &str,
    label: &str,
    badge: Option<String>,
    out: &mut Option<OpenPanel>,
    which: OpenPanel,
) {
    ui.horizontal(|ui| {
        let text = format!("{icon}  {label}");
        if ui.selectable_label(false, text).clicked() {
            *out = Some(which);
        }
        if let Some(b) = badge {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!(" {b} "))
                        .small()
                        .background_color(ui.visuals().warn_fg_color)
                        .color(egui::Color32::from_rgb(0x1a, 0x1a, 0x1a)),
                );
            });
        }
    });
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
                ui.label(egui::RichText::new(h).weak().small());
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
            for (theme, key) in [
                (Theme::Dark, "settings.theme.dark"),
                (Theme::Light, "settings.theme.light"),
                (Theme::System, "settings.theme.system"),
            ] {
                *changed |= ui
                    .selectable_value(&mut config.ui.theme, theme, catalog.t(key, &[]))
                    .changed();
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
            egui::ComboBox::from_id_salt("locale_combo")
                .selected_text(current_locale_label(&config.i18n.locale, catalog))
                .show_ui(ui, |ui| {
                    for (locale, key) in [
                        (i18n::FALLBACK_LOCALE, "settings.locale.en_us"),
                        ("ja-JP", "settings.locale.ja_jp"),
                        ("zh-Hans", "settings.locale.zh_hans"),
                        ("zh-Hant", "settings.locale.zh_hant"),
                        ("ko-KR", "settings.locale.ko_kr"),
                    ] {
                        *changed |= ui
                            .selectable_value(
                                &mut config.i18n.locale,
                                locale.to_owned(),
                                catalog.t(key, &[]),
                            )
                            .changed();
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
        *changed |= ui
            .add(
                egui::DragValue::new(&mut config.terminal.font_size)
                    .range(8.0..=32.0)
                    .speed(0.2),
            )
            .changed();
    });
    row(
        ui,
        &catalog.t("settings.scrollback_lines", &[]),
        Some(&catalog.t("settings.scrollback.hint", &[])),
        |ui| {
            *changed |= ui
                .add(
                    egui::DragValue::new(&mut config.terminal.scrollback_lines)
                        .range(1_000..=100_000)
                        .speed(50),
                )
                .changed();
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
            ui.weak(catalog.t("settings.restart_required", &[]));
            *changed |= ui
                .add(egui::DragValue::new(&mut config.performance.output_batch_ms).range(16..=50))
                .changed();
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
