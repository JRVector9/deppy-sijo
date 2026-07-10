#[derive(Debug, Clone, PartialEq)]
pub struct CredentialListItem {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub credential_kind: String,
    pub masked_hint: Option<String>,
}

pub struct NewCredential {
    pub provider: String,
    pub label: String,
    pub credential_kind: String,
    pub secret: String,
}

pub trait CredentialService {
    fn list_credentials(&self) -> anyhow::Result<Vec<CredentialListItem>>;
    fn add_credential(&self, credential: NewCredential) -> anyhow::Result<()>;
    fn delete_credential(&self, id: &str) -> anyhow::Result<()>;
    /// keyring에 남았지만 DB credentials가 모르는 항목(id 목록) — 고아 스캔.
    fn orphan_credentials(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// 고아 항목 삭제 — 지운 개수 반환.
    fn purge_orphan_credentials(&self, ids: &[String]) -> anyhow::Result<usize> {
        let _ = ids;
        Ok(0)
    }
}

/// 자격증명 관리 창 상태. secret 입력값은 추가 즉시 비운다.
pub struct CredentialsUi {
    provider: String,
    label: String,
    kind: &'static str,
    secret_input: String,
    error: Option<String>,
    /// '+ 추가' 클릭 시에만 인라인 추가 폼을 펼친다(스크린샷: 기본은 표만 — P2).
    show_add_form: bool,
    /// 고아 keyring 정리 흐름 상태 — None=대기, Some(목록)=발견(확인 대기).
    /// 삭제 확인 대기 (id, 표시명) — ×를 눌러도 바로 지우지 않고 모달로 묻는다.
    delete_confirm: Option<(String, String)>,
    orphan_candidates: Option<Vec<String>>,
    /// 마지막 정리 결과 메시지(정리 개수/없음).
    orphan_status: Option<String>,
    cached: Option<Vec<CredentialListItem>>,
    /// API 비밀키는 기본 마스킹한다. 사용자가 ○를 누른 항목만 background keyring
    /// worker 결과를 이 UI가 소유하며, 화면/목록 무효화 때 즉시 폐기한다.
    reveal_requested: std::collections::HashSet<String>,
    revealed: std::collections::HashMap<String, String>,
}

impl CredentialsUi {
    pub fn new() -> Self {
        Self {
            provider: String::new(),
            label: String::new(),
            kind: "api_key",
            secret_input: String::new(),
            error: None,
            show_add_form: false,
            delete_confirm: None,
            orphan_candidates: None,
            orphan_status: None,
            cached: None,
            reveal_requested: std::collections::HashSet::new(),
            revealed: std::collections::HashMap::new(),
        }
    }

    /// 다른 창(커넥터)이 credential을 추가했을 때 목록 캐시를 버린다.
    pub fn invalidate_cache(&mut self) {
        self.cached = None;
        self.clear_revealed_secrets();
    }

    /// 설정 닫기/워크스페이스 전환에서 목록 메타 캐시는 유지하면서 평문만 즉시 폐기한다.
    pub fn clear_revealed_secrets(&mut self) {
        self.reveal_requested.clear();
        self.revealed.clear();
    }

    /// 비-compact 버전 — settings.rs가 Credentials→Environment로 리다이렉트해 실제 도달 불가.
    /// 반환: 이 프레임에 credential 목록이 **변경**(추가/삭제 성공)됐는가 — App이 true면
    /// EnvProfilesUi 캐시를 무효화한다(PR-ENV-C 배선: env 시크릿 콤보/마스킹 stale 방지).
    pub fn contents_compact(
        &mut self,
        ui: &mut egui::Ui,
        credentials: &dyn CredentialService,
        reveal_secret: &mut dyn FnMut(&str) -> Option<String>,
        catalog: &i18n::Catalog,
    ) -> bool {
        let mut changed = false;
        let list = match &self.cached {
            Some(list) => list.clone(),
            None => match credentials.list_credentials() {
                Ok(list) => {
                    self.cached = Some(list.clone());
                    list
                }
                Err(e) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        catalog.t("common.list_failed", &[("message", &format!("{e:#}"))]),
                    );
                    return false;
                }
            },
        };

        self.reveal_requested
            .retain(|id| list.iter().any(|item| item.id == *id));
        self.revealed
            .retain(|id, _| list.iter().any(|item| item.id == *id));
        for meta in &list {
            if self.reveal_requested.contains(&meta.id)
                && !self.revealed.contains_key(&meta.id)
                && let Some(secret) = reveal_secret(&meta.id)
            {
                self.revealed.insert(meta.id.clone(), secret);
            }
        }

        // EnvVarTable의 CSS margin-bottom: 2px. SectionHeader 자체가 상단 10px
        // padding을 포함하므로 여기서 다시 10px을 더하지 않는다.
        ui.add_space(2.0);
        let add_label = format!("+ {}", catalog.t("action.add", &[]));
        if credentials_section_header(
            ui,
            &catalog.t("credentials.api_keys", &[]),
            list.len(),
            &add_label,
        ) {
            // 토글(P2) — 스크린샷은 기본 표만, 폼은 '+ 추가'를 눌렀을 때만.
            self.show_add_form = !self.show_add_form;
            if self.show_add_form {
                ui.memory_mut(|mem| mem.request_focus(credential_provider_input_id()));
            }
        }
        credentials_table_header(
            ui,
            &[
                catalog.t("credentials.provider", &[]),
                catalog.t("credentials.label", &[]),
                catalog.t("credentials.kind", &[]),
                catalog.t("credentials.secret", &[]),
            ],
        );
        let mut delete_id: Option<(String, String)> = None;
        for meta in &list {
            let row = credential_table_row(ui, meta, self.revealed.get(&meta.id), catalog);
            if row.delete {
                let name = if meta.label.is_empty() {
                    meta.provider.clone()
                } else {
                    meta.label.clone()
                };
                delete_id = Some((meta.id.clone(), name));
            }
            if row.toggle_reveal {
                if self.reveal_requested.remove(&meta.id) {
                    self.revealed.remove(&meta.id);
                } else {
                    self.reveal_requested.insert(meta.id.clone());
                }
            }
        }
        if list.is_empty() {
            ui.label(
                egui::RichText::new(catalog.t("credentials.empty", &[]))
                    .size(13.0)
                    .weak(),
            );
        }
        if let Some(pending) = delete_id {
            self.delete_confirm = Some(pending);
        }
        // 삭제 확인 모달(사용자 2026-07-10 #1) — 키체인 시크릿도 함께 지워지므로 확인 필수.
        if let Some((del_id, del_name)) = self.delete_confirm.clone() {
            let mut decision: Option<bool> = None;
            egui::Window::new(catalog.t("credentials.delete_confirm.title", &[]))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(catalog.t("credentials.delete_confirm.body", &[("name", &del_name)]));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(catalog.t("action.delete", &[])).clicked() {
                            decision = Some(true);
                        }
                        if ui.button(catalog.t("action.cancel", &[])).clicked() {
                            decision = Some(false);
                        }
                    });
                });
            match decision {
                Some(true) => {
                    match self.delete(credentials, &del_id) {
                        Ok(()) => {
                            changed = true;
                            self.reveal_requested.remove(&del_id);
                            self.revealed.remove(&del_id);
                            self.error = None;
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                    self.delete_confirm = None;
                }
                Some(false) => self.delete_confirm = None,
                None => {}
            }
        }

        if self.show_add_form {
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.provider)
                        .id_source(credential_provider_input_id())
                        .hint_text(catalog.t("credentials.provider", &[]))
                        .desired_width(120.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.label)
                        .hint_text(catalog.t("credentials.label", &[]))
                        .desired_width(160.0),
                );
                for kind in ["api_key", "token"] {
                    ui.selectable_value(&mut self.kind, kind, kind);
                }
                ui.add(
                    egui::TextEdit::singleline(&mut self.secret_input)
                        .password(true)
                        .hint_text(catalog.t("credentials.secret", &[]))
                        .desired_width(220.0),
                );
                let filled = !self.provider.trim().is_empty()
                    && !self.label.trim().is_empty()
                    && !self.secret_input.is_empty();
                if ui
                    .add_enabled(filled, egui::Button::new(catalog.t("action.add", &[])))
                    .clicked()
                {
                    match self.add(credentials) {
                        Ok(()) => {
                            changed = true;
                            self.error = None;
                        }
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
            });
        }

        // 기본 표에서는 목업에 없는 관리 링크를 숨긴다. '+ 추가' 폼을 연 경우에만
        // 고아 keyring 정리 진입점을 함께 노출해 기존 관리 기능은 보존한다.
        if self.show_add_form || self.orphan_candidates.is_some() || self.orphan_status.is_some() {
            ui.add_space(12.0);
            match self.orphan_candidates.clone() {
                None => {
                    ui.horizontal(|ui| {
                        let link = ui.add(
                            egui::Label::new(
                                egui::RichText::new(catalog.t("credentials.purge_orphans", &[]))
                                    .size(12.0)
                                    .color(ui.visuals().weak_text_color()),
                            )
                            .sense(egui::Sense::click()),
                        );
                        if link.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if link.clicked() {
                            match credentials.orphan_credentials() {
                                Ok(list) if list.is_empty() => {
                                    self.orphan_status =
                                        Some(catalog.t("credentials.purge_none", &[]));
                                }
                                Ok(list) => {
                                    self.orphan_status = None;
                                    self.orphan_candidates = Some(list);
                                }
                                Err(e) => self.orphan_status = Some(format!("{e:#}")),
                            }
                        }
                        if let Some(status) = &self.orphan_status {
                            ui.label(egui::RichText::new(status).size(12.0).weak());
                        }
                    });
                }
                Some(list) => {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(catalog.t(
                                "credentials.purge_found",
                                &[("count", &list.len().to_string())],
                            ))
                            .size(12.0)
                            .color(ui.visuals().warn_fg_color),
                        );
                        if ui
                            .small_button(catalog.t("credentials.purge_go", &[]))
                            .clicked()
                        {
                            match credentials.purge_orphan_credentials(&list) {
                                Ok(n) => {
                                    // 정리 직후 재스캔 — 남은 고아 수까지 표시(2026-07-10:
                                    // '정리됨'만 남고 0 확인이 안 되던 문제).
                                    let done = catalog
                                        .t("credentials.purge_done", &[("count", &n.to_string())]);
                                    // 재스캔 실패를 '남은 0'으로 오표시하지 않는다(codex Med).
                                    let tail = match credentials.orphan_credentials() {
                                        Ok(l) => catalog.t(
                                            "credentials.purge_remaining",
                                            &[("count", &l.len().to_string())],
                                        ),
                                        Err(e) => format!("{e:#}"),
                                    };
                                    self.orphan_status = Some(format!("{done} · {tail}"));
                                }
                                Err(e) => self.orphan_status = Some(format!("{e:#}")),
                            }
                            self.orphan_candidates = None;
                        }
                        if ui.small_button(catalog.t("action.cancel", &[])).clicked() {
                            self.orphan_candidates = None;
                        }
                    });
                }
            }
        }

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        changed
    }

    fn add(&mut self, credentials: &dyn CredentialService) -> anyhow::Result<()> {
        let request = NewCredential {
            provider: self.provider.trim().to_owned(),
            label: self.label.trim().to_owned(),
            credential_kind: self.kind.to_owned(),
            // 입력 평문은 service boundary로 옮기고 입력창은 즉시 비운다.
            secret: std::mem::take(&mut self.secret_input),
        };
        credentials.add_credential(request)?;
        self.provider.clear();
        self.label.clear();
        self.cached = None;
        Ok(())
    }

    fn delete(&mut self, credentials: &dyn CredentialService, id: &str) -> anyhow::Result<()> {
        credentials.delete_credential(id)?;
        self.cached = None;
        Ok(())
    }
}

/// 섹션 헤더: 제목 + 카운트 배지 + 우측 액션 버튼. 버튼 클릭 시 true를 반환한다.
fn credentials_section_header(
    ui: &mut egui::Ui,
    title: &str,
    count: usize,
    action_label: &str,
) -> bool {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::hover());
    let painter = ui.painter();
    let y = rect.center().y;
    let title_font = egui::FontId::monospace(14.0);
    painter.text(
        egui::pos2(rect.left(), y),
        egui::Align2::LEFT_CENTER,
        title,
        title_font.clone(),
        ui.visuals().text_color(),
    );
    let title_w = painter
        .layout_no_wrap(title.to_owned(), title_font, ui.visuals().text_color())
        .rect
        .width();
    let count_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + title_w + 16.0, y),
        egui::vec2(20.0, 20.0),
    );
    let tag = if ui.visuals().dark_mode {
        egui::Color32::from_rgb(0x2a, 0x3a, 0x44)
    } else {
        egui::Color32::from_rgb(0xd0, 0xe8, 0xf4)
    };
    painter.rect_filled(count_rect, 0.0, tag);
    painter.text(
        count_rect.center(),
        egui::Align2::CENTER_CENTER,
        count.to_string(),
        egui::FontId::monospace(12.0),
        ui.visuals().hyperlink_color,
    );

    let button_font = egui::FontId::monospace(13.0);
    let label_w = painter
        .layout_no_wrap(
            action_label.to_owned(),
            button_font.clone(),
            ui.visuals().weak_text_color(),
        )
        .rect
        .width();
    let button_w = (label_w + 16.0).max(58.0);
    let button_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - button_w / 2.0, y),
        egui::vec2(button_w, 26.0),
    );
    let response = ui.interact(
        button_rect,
        ui.id().with("credentials_section_add"),
        egui::Sense::click(),
    );
    let hovered = response.hovered();
    let fill = if hovered {
        ui.visuals().selection.bg_fill
    } else {
        ui.visuals().extreme_bg_color
    };
    let stroke = if hovered {
        ui.visuals().selection.bg_fill
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke.color
    };
    painter.rect_filled(button_rect, 0.0, fill);
    painter.rect_stroke(
        button_rect,
        0.0,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    painter.text(
        button_rect.center(),
        egui::Align2::CENTER_CENTER,
        action_label,
        button_font,
        if hovered {
            egui::Color32::WHITE
        } else {
            ui.visuals().weak_text_color()
        },
    );
    paint_credentials_hline(ui, rect.bottom());
    response.clicked()
}

/// "+ 추가" 헤더 버튼 클릭 시 포커스를 옮길 provider 입력창의 고정 Id.
fn credential_provider_input_id() -> egui::Id {
    egui::Id::new("credentials_provider_input")
}

fn credentials_table_header(ui: &mut egui::Ui, columns: &[String]) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::hover());
    let cols = credential_columns(rect);
    let painter = ui.painter();
    let color = ui.visuals().weak_text_color();
    let font = egui::FontId::monospace(12.0);
    for (idx, column) in columns.iter().enumerate() {
        painter.text(
            egui::pos2(cols[idx].left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            column,
            font.clone(),
            color,
        );
    }
    paint_credentials_hline(ui, rect.bottom());
}

struct CredentialRowResponse {
    delete: bool,
    toggle_reveal: bool,
}

fn credential_table_row(
    ui: &mut egui::Ui,
    meta: &CredentialListItem,
    revealed_secret: Option<&String>,
    catalog: &i18n::Catalog,
) -> CredentialRowResponse {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let cols = credential_columns(rect);
    let painter = ui.painter();
    let y = rect.center().y;
    painter.with_clip_rect(cols[0]).text(
        egui::pos2(cols[0].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        &meta.provider,
        egui::FontId::monospace(14.0),
        credential_secondary_text(ui),
    );
    painter.with_clip_rect(cols[1]).text(
        egui::pos2(cols[1].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        &meta.label,
        egui::FontId::monospace(14.0),
        ui.visuals().text_color(),
    );
    if let Some(badge_width) = credential_kind_badge_width(&meta.credential_kind, cols[2].width()) {
        let badge = egui::Rect::from_min_size(
            egui::pos2(cols[2].left() + 4.0, y - 12.0),
            egui::vec2(badge_width, 24.0),
        );
        // 종류 뱃지(P5, 스크린샷): token = accent 채움+대비 글자, api_key 등 = 1px outline.
        let filled = meta.credential_kind == "token";
        if filled {
            painter.rect_filled(badge, 0.0, ui.visuals().selection.bg_fill);
        } else {
            painter.rect_stroke(
                badge,
                0.0,
                egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
                egui::StrokeKind::Inside,
            );
        }
        let badge_text_color = if filled {
            egui::Color32::WHITE
        } else {
            ui.visuals().weak_text_color()
        };
        painter.with_clip_rect(cols[2]).text(
            egui::pos2(badge.left() + 5.0, badge.center().y),
            egui::Align2::LEFT_CENTER,
            &meta.credential_kind,
            egui::FontId::monospace(12.0),
            badge_text_color,
        );
    }
    let reveal_center = egui::pos2(cols[3].right() - 9.0, y);
    let reveal_rect = egui::Rect::from_center_size(reveal_center, egui::vec2(18.0, 16.0));
    let secret_clip = egui::Rect::from_min_max(
        cols[3].min,
        egui::pos2(
            (reveal_rect.left() - 2.0).max(cols[3].left()),
            cols[3].bottom(),
        ),
    );
    painter.with_clip_rect(secret_clip).text(
        egui::pos2(cols[3].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        revealed_secret
            .map(String::as_str)
            .unwrap_or("••••••••••••••••"),
        egui::FontId::monospace(14.0),
        ui.visuals().text_color(),
    );

    let reveal = ui
        .interact(
            reveal_rect,
            ui.id().with(("credential_reveal", &meta.id)),
            egui::Sense::click(),
        )
        .on_hover_text(if revealed_secret.is_some() {
            catalog.t("action.hide_secret", &[])
        } else {
            catalog.t("action.show_secret", &[])
        });
    if response.hovered() {
        painter.rect_stroke(
            reveal_rect,
            0.0,
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
    }
    painter.text(
        reveal_center,
        egui::Align2::CENTER_CENTER,
        if revealed_secret.is_some() {
            "●"
        } else {
            "○"
        },
        egui::FontId::monospace(12.0),
        if revealed_secret.is_some() {
            ui.visuals().hyperlink_color
        } else {
            ui.visuals().weak_text_color()
        },
    );

    let delete_rect = egui::Rect::from_center_size(cols[4].center(), egui::vec2(22.0, 18.0));
    let delete = ui
        .interact(
            delete_rect,
            ui.id().with(("credential_delete", &meta.id)),
            egui::Sense::click(),
        )
        .on_hover_text(catalog.t("action.delete", &[]));
    let hovered = delete.hovered();
    let stroke = if hovered {
        ui.visuals().error_fg_color
    } else if response.hovered() {
        ui.visuals().widgets.noninteractive.bg_stroke.color
    } else {
        egui::Color32::TRANSPARENT
    };
    let fill = if hovered {
        ui.visuals().error_fg_color
    } else {
        egui::Color32::TRANSPARENT
    };
    let text = if hovered {
        ui.visuals().window_fill
    } else {
        ui.visuals().weak_text_color()
    };
    painter.rect_filled(delete_rect, 0.0, fill);
    painter.rect_stroke(
        delete_rect,
        0.0,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    painter.text(
        delete_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::monospace(12.0),
        text,
    );
    paint_credentials_hline(ui, rect.top());
    paint_credentials_hline(ui, rect.bottom());
    CredentialRowResponse {
        delete: delete.clicked(),
        toggle_reveal: reveal.clicked(),
    }
}

fn credential_columns(rect: egui::Rect) -> [egui::Rect; 5] {
    const GAP: f32 = 4.0;
    const PROVIDER_W: f32 = 80.0;
    const KIND_W: f32 = 56.0;
    const ACTION_W: f32 = 24.0;
    let flexible = ((rect.width() - PROVIDER_W - KIND_W - ACTION_W - GAP * 4.0).max(0.0)) / 2.0;
    let provider = egui::Rect::from_min_size(rect.min, egui::vec2(PROVIDER_W, rect.height()));
    let label = egui::Rect::from_min_size(
        egui::pos2(provider.right() + GAP, rect.top()),
        egui::vec2(flexible, rect.height()),
    );
    let kind = egui::Rect::from_min_size(
        egui::pos2(label.right() + GAP, rect.top()),
        egui::vec2(KIND_W, rect.height()),
    );
    let secret = egui::Rect::from_min_size(
        egui::pos2(kind.right() + GAP, rect.top()),
        egui::vec2(flexible, rect.height()),
    );
    let delete = egui::Rect::from_min_size(
        egui::pos2(secret.right() + GAP, rect.top()),
        egui::vec2(ACTION_W, rect.height()),
    );
    [provider, label, kind, secret, delete]
}

fn credential_secondary_text(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_rgb(0xaa, 0xaa, 0xaa)
    } else {
        egui::Color32::from_rgb(0x44, 0x44, 0x44)
    }
}

fn credential_kind_badge_width(kind: &str, column_width: f32) -> Option<f32> {
    if !column_width.is_finite() || column_width <= 14.0 {
        return None;
    }
    let desired = kind.len() as f32 * 8.0 + 14.0;
    let max = (column_width - 8.0).max(14.0);
    Some(desired.min(max))
}

fn paint_credentials_hline(ui: &egui::Ui, y: f32) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let y = ui.painter().round_to_pixel_center(y);
    ui.painter()
        .hline(ui.min_rect().x_range(), y, egui::Stroke::new(1.0, color));
}

#[cfg(test)]
mod tests {
    use super::{credential_columns, credential_kind_badge_width};

    #[test]
    fn credential_badge_width는_좁은_컬럼에서_panic하지_않는다() {
        assert_eq!(credential_kind_badge_width("api_key", -8.0), None);
        assert_eq!(credential_kind_badge_width("api_key", 0.0), None);
        assert_eq!(credential_kind_badge_width("api_key", 14.0), None);
        assert_eq!(credential_kind_badge_width("api_key", f32::NAN), None);
        assert_eq!(credential_kind_badge_width("api_key", 22.0), Some(14.0));
    }

    #[test]
    fn credential_columns는_참조_grid_폭을_유지한다() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(841.0, 30.0));
        let columns = credential_columns(rect);
        assert_eq!(columns[0].width(), 80.0);
        assert_eq!(columns[1].width(), 332.5);
        assert_eq!(columns[2].width(), 56.0);
        assert_eq!(columns[3].width(), 332.5);
        assert_eq!(columns[4].width(), 24.0);
        assert_eq!(columns[4].right(), rect.right());
    }
}
