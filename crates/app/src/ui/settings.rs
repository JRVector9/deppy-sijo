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

/// 설정 창 결과.
pub struct SettingsOutput {
    /// config 값이 바뀌어 저장이 필요한가 (테마/터미널/성능/포트).
    pub config_changed: bool,
    /// Remote 섹션 동작 요청.
    pub remote_action: RemoteAction,
}

/// 설정 창. config 변경 여부와 Remote 섹션 동작을 [`SettingsOutput`]으로 돌려준다.
pub fn show(
    ctx: &egui::Context,
    open: &mut bool,
    config: &mut Config,
    remote: &RemoteView,
    reveal_token: &mut bool,
    catalog: &i18n::Catalog,
) -> SettingsOutput {
    let mut changed = false;
    let mut remote_action = RemoteAction::None;
    egui::Window::new(catalog.t("settings.title", &[]))
        .open(open)
        .resizable(false)
        .show(ctx, |ui| {
            ui.heading(catalog.t("settings.ui", &[]));
            ui.horizontal(|ui| {
                ui.label(catalog.t("settings.theme", &[]));
                for (theme, label_key) in [
                    (Theme::System, "settings.theme.system"),
                    (Theme::Light, "settings.theme.light"),
                    (Theme::Dark, "settings.theme.dark"),
                ] {
                    changed |= ui
                        .selectable_value(&mut config.ui.theme, theme, catalog.t(label_key, &[]))
                        .changed();
                }
            });
            // 폴더 트리 사이드바 ON/OFF (file-tree-design §6) — hot toggle, OFF면 리소스 0
            changed |= ui
                .checkbox(
                    &mut config.ui.file_tree_enabled,
                    catalog.t("settings.file_tree_sidebar", &[]),
                )
                .changed();

            ui.separator();
            ui.heading(catalog.t("settings.language", &[]));
            ui.horizontal_wrapped(|ui| {
                ui.label(catalog.t("settings.locale", &[]));
                for (locale, label_key) in [
                    (i18n::FALLBACK_LOCALE, "settings.locale.en_us"),
                    ("ja-JP", "settings.locale.ja_jp"),
                    ("zh-Hans", "settings.locale.zh_hans"),
                    ("zh-Hant", "settings.locale.zh_hant"),
                    ("ko-KR", "settings.locale.ko_kr"),
                    (i18n::PSEUDO_LOCALE, "settings.locale.pseudo"),
                ] {
                    changed |= ui
                        .selectable_value(
                            &mut config.i18n.locale,
                            locale.to_owned(),
                            catalog.t(label_key, &[]),
                        )
                        .changed();
                }
            });

            ui.separator();
            ui.heading(catalog.t("settings.terminal", &[]));
            ui.horizontal(|ui| {
                ui.label(catalog.t("settings.font_size", &[]));
                changed |= ui
                    .add(egui::DragValue::new(&mut config.terminal.font_size).range(8.0..=32.0))
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label(catalog.t("settings.scrollback_lines", &[]));
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut config.terminal.scrollback_lines)
                            .range(1_000..=100_000),
                    )
                    .changed();
            });

            ui.separator();
            ui.heading(catalog.t("settings.performance", &[]));
            ui.horizontal(|ui| {
                ui.label(catalog.t("settings.output_batch_ms", &[]));
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut config.performance.output_batch_ms)
                            .range(16..=50),
                    )
                    .changed();
                ui.weak(catalog.t("settings.restart_required", &[]));
            });

            ui.separator();
            ui.heading(catalog.t("settings.remote_tls", &[]));
            // 체크박스 = 실행 중 OR 저장된 자동시작 의도. 자동시작이 실패해도 켜진 채(+에러
            // 표시)로 남아, 사용자가 꺼서 persisted auto-start를 해제할 수 있다 (codex Medium —
            // running만 반영하면 실패 상태에서 Start만 나가 auto-start를 UI로 끌 수 없음).
            let mut enabled = remote.running || config.remote.tls_enabled;
            if ui
                .checkbox(&mut enabled, catalog.t("settings.remote_tls_enabled", &[]))
                .changed()
            {
                remote_action = if enabled {
                    RemoteAction::Start
                } else {
                    RemoteAction::Stop
                };
            }
            ui.horizontal(|ui| {
                ui.label(catalog.t("settings.port", &[]));
                changed |= ui
                    .add(egui::DragValue::new(&mut config.remote.port).range(0..=65535))
                    .changed();
                ui.weak(catalog.t("settings.toggle_restart_required", &[]));
            });
            if let Some(err) = remote.error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    catalog.t("settings.start_failed", &[("message", err)]),
                );
            }
            if remote.running {
                if let Some(addr) = &remote.addr {
                    ui.horizontal(|ui| {
                        ui.label(catalog.t("settings.address", &[]));
                        ui.add(
                            egui::Label::new(egui::RichText::new(addr).monospace())
                                .selectable(true),
                        );
                    });
                }
                if let Some(fp) = remote.fingerprint {
                    ui.label(catalog.t("settings.fingerprint", &[]));
                    ui.add(
                        egui::Label::new(egui::RichText::new(fp).monospace())
                            .selectable(true)
                            .wrap(),
                    );
                }
                if let Some(token) = remote.token {
                    ui.horizontal(|ui| {
                        ui.label(catalog.t("settings.token", &[]));
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

            ui.separator();
            ui.heading(catalog.t("settings.known_hosts", &[]));
            ui.weak(remote.known_hosts_path.as_str());
            if remote.known_hosts.is_empty() {
                ui.weak(catalog.t("settings.no_trust_records", &[]));
            } else {
                for (host, fp) in remote.known_hosts {
                    ui.horizontal(|ui| {
                        ui.monospace(host);
                        ui.weak(truncate_fingerprint(fp, 17));
                        if ui.button(catalog.t("settings.forget", &[])).clicked() {
                            remote_action = RemoteAction::Forget(host.clone());
                        }
                    });
                }
            }
        });
    SettingsOutput {
        config_changed: changed,
        remote_action,
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
    use super::truncate_fingerprint;

    #[test]
    fn 지문_짧으면_그대로() {
        assert_eq!(truncate_fingerprint("ab:cd", 10), "ab:cd");
    }

    #[test]
    fn 지문_길면_앞부분만_말줄임() {
        assert_eq!(truncate_fingerprint("aa:bb:cc:dd", 5), "aa:bb…");
    }
}
