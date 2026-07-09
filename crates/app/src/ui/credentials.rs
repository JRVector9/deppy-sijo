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
}

/// 자격증명 관리 창 상태. secret 입력값은 추가 즉시 비운다.
pub struct CredentialsUi {
    provider: String,
    label: String,
    kind: &'static str,
    secret_input: String,
    error: Option<String>,
    cached: Option<Vec<CredentialListItem>>,
}

impl CredentialsUi {
    pub fn new() -> Self {
        Self {
            provider: String::new(),
            label: String::new(),
            kind: "api_key",
            secret_input: String::new(),
            error: None,
            cached: None,
        }
    }

    /// 다른 창(커넥터)이 credential을 추가했을 때 목록 캐시를 버린다.
    pub fn invalidate_cache(&mut self) {
        self.cached = None;
    }

    /// 비-compact 버전 — settings.rs가 Credentials→Environment로 리다이렉트해 실제 도달 불가.
    /// app.rs C::Credentials 분기가 아직 호출하므로 유지(분기 제거 시 함께 삭제).
    #[allow(dead_code)]
    pub fn contents(
        &mut self,
        ui: &mut egui::Ui,
        credentials: &dyn CredentialService,
        catalog: &i18n::Catalog,
    ) {
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
                    return;
                }
            },
        };

        if list.is_empty() {
            ui.label(catalog.t("credentials.empty", &[]));
        }
        let mut delete_id = None;
        for meta in &list {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} · {} ({}) {}",
                    meta.label,
                    meta.provider,
                    meta.credential_kind,
                    meta.masked_hint.as_deref().unwrap_or(""),
                ));
                if ui.button(catalog.t("action.delete", &[])).clicked() {
                    delete_id = Some(meta.id.clone());
                }
            });
        }
        if let Some(id) = delete_id {
            self.error = self
                .delete(credentials, &id)
                .err()
                .map(|e| format!("{e:#}"));
        }

        ui.separator();
        ui.heading(catalog.t("credentials.add", &[]));
        // 좌측 라벨 컬럼 + 넓은 입력 폼 — 라벨 far-left/입력 far-right로 큰 빈 공간이 생기던
        // 우측정렬을 폼 스타일로 교체(#5 레이아웃 수정). 모든 입력이 같은 x에서 시작해 정렬.
        const LABEL_W: f32 = 88.0;
        const FIELD_W: f32 = 320.0;
        let text_color = ui.visuals().text_color();
        let field_row = |ui: &mut egui::Ui, label: String, add: &mut dyn FnMut(&mut egui::Ui)| {
            ui.horizontal(|ui| {
                let (r, _) =
                    ui.allocate_exact_size(egui::vec2(LABEL_W, 30.0), egui::Sense::hover());
                ui.painter().text(
                    egui::pos2(r.left(), r.center().y),
                    egui::Align2::LEFT_CENTER,
                    label,
                    egui::FontId::proportional(13.5),
                    text_color,
                );
                add(ui);
            });
        };
        field_row(ui, catalog.t("credentials.provider", &[]), &mut |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.provider).desired_width(FIELD_W));
        });
        field_row(ui, catalog.t("credentials.label", &[]), &mut |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.label).desired_width(FIELD_W));
        });
        field_row(ui, catalog.t("credentials.kind", &[]), &mut |ui| {
            for kind in ["api_key", "token"] {
                ui.selectable_value(&mut self.kind, kind, kind);
            }
        });
        field_row(ui, catalog.t("credentials.secret", &[]), &mut |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.secret_input)
                    .password(true)
                    .desired_width(FIELD_W),
            );
        });
        let filled = !self.provider.trim().is_empty()
            && !self.label.trim().is_empty()
            && !self.secret_input.is_empty();
        if ui
            .add_enabled(filled, egui::Button::new(catalog.t("action.add", &[])))
            .clicked()
        {
            self.error = self.add(credentials).err().map(|e| format!("{e:#}"));
        }

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }

    pub fn contents_compact(
        &mut self,
        ui: &mut egui::Ui,
        credentials: &dyn CredentialService,
        catalog: &i18n::Catalog,
    ) {
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
                    return;
                }
            },
        };

        ui.add_space(16.0);
        if credentials_section_header(
            ui,
            &catalog.t("credentials.api_keys", &[]),
            list.len(),
            &catalog.t("env.add_key", &[]),
        ) {
            ui.memory_mut(|mem| mem.request_focus(credential_provider_input_id()));
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
        let mut delete_id = None;
        for meta in &list {
            if credential_table_row(ui, meta, catalog) {
                delete_id = Some(meta.id.clone());
            }
            credentials_table_divider(ui);
        }
        if list.is_empty() {
            ui.label(
                egui::RichText::new(catalog.t("credentials.empty", &[]))
                    .size(13.0)
                    .weak(),
            );
        }
        if let Some(id) = delete_id {
            self.error = self
                .delete(credentials, &id)
                .err()
                .map(|e| format!("{e:#}"));
        }

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
                self.error = self.add(credentials).err().map(|e| format!("{e:#}"));
            }
        });

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
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
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).size(14.0).strong());
        egui::Frame::NONE
            .fill(ui.visuals().faint_bg_color)
            .inner_margin(egui::Margin::symmetric(6, 2))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(count.to_string())
                        .size(13.0)
                        .color(ui.visuals().hyperlink_color),
                );
            });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // sharp(라운딩 0) — 1px border는 테마 기본 widget stroke를 그대로 사용.
            clicked = ui
                .add(egui::Button::new(action_label).corner_radius(0))
                .clicked();
        });
    });
    clicked
}

/// "+ 추가" 헤더 버튼 클릭 시 포커스를 옮길 provider 입력창의 고정 Id.
fn credential_provider_input_id() -> egui::Id {
    egui::Id::new("credentials_provider_input")
}

fn credentials_table_header(ui: &mut egui::Ui, columns: &[String]) {
    ui.add_space(2.0);
    credentials_table_divider(ui);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    let cols = credential_columns(rect);
    let painter = ui.painter();
    let color = ui.visuals().weak_text_color();
    let font = egui::FontId::proportional(12.0);
    for (idx, column) in columns.iter().enumerate() {
        painter.text(
            egui::pos2(cols[idx].left() + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            column,
            font.clone(),
            color,
        );
    }
    credentials_table_divider(ui);
}

fn credential_table_row(
    ui: &mut egui::Ui,
    meta: &CredentialListItem,
    catalog: &i18n::Catalog,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::hover());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let cols = credential_columns(rect);
    let painter = ui.painter();
    let y = rect.center().y;
    painter.with_clip_rect(cols[0]).text(
        egui::pos2(cols[0].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        &meta.provider,
        egui::FontId::proportional(14.0),
        ui.visuals().text_color(),
    );
    painter.with_clip_rect(cols[1]).text(
        egui::pos2(cols[1].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        &meta.label,
        egui::FontId::proportional(14.0),
        ui.visuals().text_color(),
    );
    if let Some(badge_width) = credential_kind_badge_width(&meta.credential_kind, cols[2].width()) {
        let badge = egui::Rect::from_min_size(
            egui::pos2(cols[2].left() + 6.0, y - 14.0),
            egui::vec2(badge_width, 28.0),
        );
        painter.rect_stroke(
            badge,
            0.0,
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
        painter.with_clip_rect(cols[2]).text(
            egui::pos2(badge.left() + 7.0, badge.center().y),
            egui::Align2::LEFT_CENTER,
            &meta.credential_kind,
            egui::FontId::proportional(13.0),
            ui.visuals().weak_text_color(),
        );
    }
    painter.with_clip_rect(cols[3]).text(
        egui::pos2(cols[3].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        meta.masked_hint.as_deref().unwrap_or("••••••••••••"),
        egui::FontId::proportional(14.0),
        ui.visuals().text_color(),
    );

    // #6: 안전한 reveal 경로가 없어 죽어있던 ○ 버튼은 제거하고 삭제(×)만 남긴다.
    let delete_rect =
        egui::Rect::from_center_size(egui::pos2(rect.right() - 24.0, y), egui::vec2(28.0, 20.0));
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
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke.color
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
        egui::FontId::proportional(12.0),
        text,
    );
    delete.clicked()
}

fn credential_columns(rect: egui::Rect) -> [egui::Rect; 4] {
    // reveal(○) 버튼 제거로 삭제(×) 하나만 남아 액션 열 폭을 축소했다(#6).
    let action_w = 48.0;
    let content = egui::Rect::from_min_max(
        rect.min,
        egui::pos2((rect.right() - action_w).max(rect.left()), rect.bottom()),
    );
    let w = content.width();
    let provider_w = w * 0.16;
    let label_w = w * 0.32;
    let kind_w = w * 0.18;
    let provider = egui::Rect::from_min_size(content.min, egui::vec2(provider_w, content.height()));
    let label = provider.translate(egui::vec2(provider_w, 0.0));
    let label = egui::Rect::from_min_size(label.min, egui::vec2(label_w, content.height()));
    let kind = label.translate(egui::vec2(label_w, 0.0));
    let kind = egui::Rect::from_min_size(kind.min, egui::vec2(kind_w, content.height()));
    let secret = egui::Rect::from_min_max(
        egui::pos2(kind.right(), content.top()),
        egui::pos2(content.right(), content.bottom()),
    );
    [provider, label, kind, secret]
}

fn credential_kind_badge_width(kind: &str, column_width: f32) -> Option<f32> {
    if !column_width.is_finite() || column_width <= 14.0 {
        return None;
    }
    let desired = kind.len() as f32 * 8.0 + 14.0;
    let max = (column_width - 8.0).max(14.0);
    Some(desired.min(max))
}

fn credentials_table_divider(ui: &mut egui::Ui) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    let y = ui.painter().round_to_pixel_center(rect.center().y);
    ui.painter()
        .hline(rect.x_range(), y, egui::Stroke::new(1.0, color));
}

#[cfg(test)]
mod tests {
    use super::credential_kind_badge_width;

    #[test]
    fn credential_badge_width는_좁은_컬럼에서_panic하지_않는다() {
        assert_eq!(credential_kind_badge_width("api_key", -8.0), None);
        assert_eq!(credential_kind_badge_width("api_key", 0.0), None);
        assert_eq!(credential_kind_badge_width("api_key", 14.0), None);
        assert_eq!(credential_kind_badge_width("api_key", f32::NAN), None);
        assert_eq!(credential_kind_badge_width("api_key", 22.0), Some(14.0));
    }
}
