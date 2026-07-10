use crate::env::EnvValue;
use crate::storage::{CredentialMeta, Db, EnvProfileRow, EnvVarRow};

/// 환경 UI에서 App으로 올라가는 액션.
pub enum EnvAction {
    /// 프로젝트 폴더(워크스페이스 path) 설정 — App이 .env 재동기화 + 파일트리 루트 갱신.
    SetProjectPath(std::path::PathBuf),
    /// 프로젝트 env 재탐색(리프레시 ⟳) — .env 계열 재스캔 + 캐시 무효화(2026-07-10).
    Resync,
    /// dotenv profile 편집을 .env 파일에 반영(7·8번) — value None이면 라인 삭제.
    DotenvWrite { key: String, value: Option<String> },
}

/// 프로젝트 환경(env profile) 관리 창.
pub struct EnvProfilesUi {
    selected: Option<String>,
    new_name: String,
    new_kind: &'static str,
    var_key: String,
    var_is_secret: bool,
    var_plain_value: String,
    var_credential_id: Option<String>,
    error: Option<String>,
    /// '+ 추가' 클릭 시에만 인라인 추가 폼을 펼친다(스크린샷: 기본은 표만 — P2).
    show_add_form: bool,
    /// 삭제 확인 대기 (profile_id, key) — ×를 눌러도 바로 지우지 않고 모달로 묻는다.
    /// profile_id를 함께 저장해 확인 중 프로파일 전환 시 다른 프로파일의 같은 key가
    /// 지워지는 것을 막는다(codex High 2026-07-10).
    delete_confirm: Option<(String, String)>,
    /// 프로파일 관리 UI 수동 펼침 — 기본 숨김(P1)이어도 '프로파일 관리…' 링크로 접근
    /// 가능(단일 프로파일에서 두 번째 생성 경로 보존 — codex Med).
    show_profile_controls: bool,
    profiles: Option<Vec<EnvProfileRow>>,
    vars: Option<Vec<EnvVarRow>>,
    /// secret 평문 캐시 — vars 로드 시 일괄 resolve(keyring, 캐시 미스 프레임 1회).
    /// **기본이 노출**(개인 로컬 서비스, 사용자 2026-07-09)이라 렌더마다 조회하지 않도록
    /// 캐시한다. 전환/삭제/동기화 시 비움.
    revealed: std::collections::HashMap<(String, String), String>,
    /// dot(●) 토글로 **가린** 키들 — 기본은 노출, 켜면 마스킹.
    masked: std::collections::HashSet<(String, String)>,
    /// credential 메타 캐시 — 매 프레임 list_credentials() 동기 SQLite 조회 방지.
    /// credential은 workspace와 무관한 전역 데이터라 workspace 전환 시 버리지 않는다.
    credentials: Option<Vec<CredentialMeta>>,
    /// 캐시가 속한 workspace — 다른 workspace로 바뀌면 캐시/선택을 통째로 버린다
    /// (§6.1 "프로젝트 A 키가 B에 들어감" 방지 — codex 리뷰)
    cached_workspace: Option<String>,
}

impl EnvProfilesUi {
    /// 같은 workspace에서 DB가 외부 경로(.env 자동 동기화 등)로 바뀌었을 때 목록/변수
    /// 캐시를 버린다 — 다음 프레임에 DB에서 재조회(상세 표가 stale/카운트 불일치 방지).
    pub fn invalidate_cache(&mut self) {
        self.profiles = None;
        self.vars = None;
        self.delete_confirm = None;
        // 외부 .env 동기화가 credential을 새로 만들 수 있으므로 함께 버린다.
        self.credentials = None;
        // .env 동기화가 같은 (profile,key)의 secret 값을 바꿨을 수 있다 — 평문 캐시를
        // 비워 다음 로드에서 재resolve(codex 2026-07-09). 가림 토글은 유지.
        self.revealed.clear();
    }

    /// 추가 폼 draft를 버린다 — 워크스페이스/프로파일이 바뀌면 이전 컨텍스트의 입력이
    /// 다른 대상에 저장되는 누수를 막는다(codex Med).
    fn reset_var_form(&mut self) {
        self.show_add_form = false;
        self.revealed.clear();
        self.masked.clear();
        self.var_key.clear();
        self.var_plain_value.clear();
        self.var_credential_id = None;
    }

    pub fn new() -> Self {
        Self {
            selected: None,
            new_name: String::new(),
            new_kind: "local",
            var_key: String::new(),
            var_is_secret: false,
            var_plain_value: String::new(),
            var_credential_id: None,
            error: None,
            revealed: std::collections::HashMap::new(),
            masked: std::collections::HashSet::new(),
            show_add_form: false,
            delete_confirm: None,
            show_profile_controls: false,
            profiles: None,
            vars: None,
            credentials: None,
            cached_workspace: None,
        }
    }

    pub fn contents_compact(
        &mut self,
        ui: &mut egui::Ui,
        db: &mut Db,
        workspace_id: &str,
        reveal_secret: &mut dyn FnMut(&str) -> Option<String>,
        catalog: &i18n::Catalog,
    ) -> anyhow::Result<Option<EnvAction>> {
        if self.cached_workspace.as_deref() != Some(workspace_id) {
            self.profiles = None;
            self.vars = None;
            self.selected = None;
            self.cached_workspace = Some(workspace_id.to_owned());
            // 이전 워크스페이스에서 펼친 추가 폼/입력값이 넘어와 엉뚱한 곳에 저장되지 않게.
            self.reset_var_form();
            self.show_profile_controls = false;
            // v19(#2)부터 credential 목록이 workspace별(소속+전역) — 이전 워크스페이스
            // 목록이 콤보에 남아 교차 참조로 저장되지 않게 캐시를 버린다(codex High).
            self.credentials = None;
        }

        let profiles = match &self.profiles {
            Some(p) => p.clone(),
            None => {
                let p = db.list_env_profiles(workspace_id)?;
                self.profiles = Some(p.clone());
                p
            }
        };

        if self.selected.is_none()
            || !profiles
                .iter()
                .any(|p| Some(p.id.as_str()) == self.selected.as_deref())
        {
            self.selected = profiles.first().map(|p| p.id.clone());
            self.vars = None;
        }

        if profiles.is_empty() {
            // 프로파일이 없으면 생성 폼만 (환경 변수 섹션 진입 전).
            compact_profile_form(ui, self, db, workspace_id, catalog)?;
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            return Ok(None);
        }

        // 경고 표시용 production 플래그(선택 변경 프레임엔 1프레임 stale — 무해).
        let pre_production = self
            .selected
            .as_deref()
            .and_then(|id| profiles.iter().find(|p| p.id == id))
            .map(|p| p.is_production)
            .unwrap_or(false);

        // 프로파일 선택기(#5, 상단): 스크린샷은 프로젝트당 환경이 암묵적 1개라 노출하지
        // 않는다 — **2개 이상이거나 production일 때만** 표시(관리 기능 보존, P1).
        // 이 안에서 선택 변경/삭제 시 self.selected/self.vars가 바뀔 수 있으므로 아래에서
        // profile_id·vars를 **재확정**한다(codex High — stale 캐시 오표시/오삭제 방지).
        let controls_visible = profiles.len() > 1 || pre_production || self.show_profile_controls;
        if controls_visible {
            compact_profile_controls(
                ui,
                self,
                db,
                workspace_id,
                &profiles,
                pre_production,
                catalog,
            )?;
            ui.add_space(14.0);
        }

        // controls가 프로파일을 생성/삭제하면 self.profiles=None로 만든다 — 그 경우 stale
        // 스냅샷으로 삭제된 id를 재선택하지 않게 **최신 목록을 재조회**한다(codex High).
        let profiles = match &self.profiles {
            Some(p) => p.clone(),
            None => {
                let p = db.list_env_profiles(workspace_id)?;
                self.profiles = Some(p.clone());
                p
            }
        };
        // 재확정 — 선택이 바뀌었거나 삭제됐으면 first로 폴백. 목록이 비면(마지막 삭제) 종료.
        if self.selected.is_none()
            || !profiles
                .iter()
                .any(|p| Some(p.id.as_str()) == self.selected.as_deref())
        {
            self.selected = profiles.first().map(|p| p.id.clone());
            self.vars = None;
            self.reset_var_form();
        }
        let Some(profile_id) = self.selected.clone() else {
            return Ok(None);
        };
        let mut action: Option<EnvAction> = None;
        let is_dotenv = profiles
            .iter()
            .find(|p| p.id == profile_id)
            .map(|p| p.kind == "dotenv")
            .unwrap_or(false);
        if !profiles.iter().any(|p| p.id == profile_id) {
            return Ok(None);
        }

        let credentials = match &self.credentials {
            Some(c) => c.clone(),
            None => {
                let c = db.list_credentials_for_workspace(workspace_id)?;
                self.credentials = Some(c.clone());
                c
            }
        };
        let vars = match &self.vars {
            Some(v) => v.clone(),
            None => {
                let v = db.list_env_vars(&profile_id)?;
                // 외부 .env 동기화로 사라진 키의 마스킹 tombstone을 계속 들고 있으면,
                // 같은 workspace에서 키 이름이 계속 바뀌는 동안 HashSet이 제한 없이 자란다.
                // 현재 프로파일에 실제로 남은 키만 유지한다.
                let live_keys: std::collections::HashSet<&str> =
                    v.iter().map(|var| var.key.as_str()).collect();
                self.masked.retain(|(masked_profile, key)| {
                    masked_profile != &profile_id || live_keys.contains(key.as_str())
                });
                self.vars = Some(v.clone());
                v
            }
        };
        // 기본 노출 정책은 유지하되, callback은 App의 background keyring worker에 요청만
        // 넣는다. 결과가 도착한 다음 프레임에도 vars cache가 Some이므로 매 프레임 아직
        // 비어 있는 항목만 확인해야 평문 cache가 채워진다.
        for var in &vars {
            if let EnvValue::Secret { credential_id } = &var.value {
                let id = (profile_id.clone(), var.key.clone());
                if !self.revealed.contains_key(&id)
                    && let Some(plain) = reveal_secret(credential_id)
                {
                    self.revealed.insert(id, plain);
                }
            }
        }

        // 환경 변수: api-like 분리 없이 **전부 한 표**로(#1/#4). API 키는 App이 별도
        // 자격증명 섹션으로 렌더한다 — 여기서 두 번째 "API Keys" 섹션은 만들지 않는다.
        if env_api_section_header(
            ui,
            &catalog.t("env.env_vars", &[]),
            Some(vars.len()),
            Some(&catalog.t("env.add_key", &[])),
        ) {
            // 토글(P2) — 스크린샷은 기본 표만, 폼은 '+ 추가'를 눌렀을 때만.
            self.show_add_form = !self.show_add_form;
            if self.show_add_form {
                ui.memory_mut(|mem| mem.request_focus(env_var_key_input_id()));
            }
        }
        env_table_header(
            ui,
            &[catalog.t("common.key", &[]), catalog.t("common.value", &[])],
        );
        let mut delete_key = None;
        let mut toggle_mask: Option<String> = None;
        for var in &vars {
            let reveal_id = (profile_id.clone(), var.key.clone());
            let is_masked = self.masked.contains(&reveal_id);
            // 기본 노출 — 가림 토글이 켜진 행만 마스킹(사용자 2026-07-09).
            let revealed_value = if is_masked {
                None
            } else {
                self.revealed.get(&reveal_id).map(String::as_str)
            };
            let row = env_table_row(ui, var, &credentials, revealed_value, is_masked, catalog);
            if row.delete {
                self.delete_confirm = Some((profile_id.clone(), var.key.clone()));
            }
            if row.toggle_reveal {
                toggle_mask = Some(var.key.clone());
            }
            env_table_divider(ui);
        }
        if let Some(key) = toggle_mask {
            let id = (profile_id.clone(), key);
            if !self.masked.remove(&id) {
                self.masked.insert(id);
            }
        }
        if vars.is_empty() {
            if env_empty_placeholder_row(ui, catalog) {
                self.show_add_form = true;
            }
            env_table_divider(ui);
        }

        if self.show_add_form
            && let Some(written) =
                compact_env_var_form(ui, self, db, &profile_id, is_dotenv, &credentials, catalog)?
        {
            // 7번(2026-07-10): dotenv profile 추가/수정은 .env 파일에도 기록 —
            // 안 쓰면 다음 동기화 때 파일 기준으로 지워진다(.env가 진실).
            action = Some(EnvAction::DotenvWrite {
                key: written.0,
                value: Some(written.1),
            });
        }

        // 삭제 확인 모달(사용자 2026-07-10 #1) — 확정 시에만 delete_key로 진행.
        // egui::Window는 배경 상호작용을 막지 않으므로, 대기 중 프로파일이 바뀌면 폐기.
        if let Some((pending_profile, _)) = &self.delete_confirm
            && *pending_profile != profile_id
        {
            self.delete_confirm = None;
        }
        if let Some((_, pending)) = self.delete_confirm.clone() {
            let mut decision: Option<bool> = None;
            egui::Window::new(catalog.t("env.var_delete_confirm.title", &[]))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    let body_key = if is_dotenv {
                        "env.var_delete_confirm.body_dotenv"
                    } else {
                        "env.var_delete_confirm.body"
                    };
                    ui.label(catalog.t(body_key, &[("key", &pending)]));
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
                    delete_key = Some(pending);
                    self.delete_confirm = None;
                }
                Some(false) => self.delete_confirm = None,
                None => {}
            }
        }

        if let Some(key) = delete_key {
            let id = (profile_id.clone(), key.clone());
            self.revealed.remove(&id);
            self.masked.remove(&id); // 재추가 시 '기본 노출'이 tombstone에 가려지지 않게
            self.error = None;
            if is_dotenv {
                // 8번(codex Med 반영): DB를 먼저 지우지 않는다 — 파일 라인 제거 후
                // sync의 '사라진 키 제거'가 DB row와 전용 credential/keyring까지 일관
                // 정리한다(먼저 지우면 sync가 credential 회수 기회를 잃음).
                action = Some(EnvAction::DotenvWrite { key, value: None });
            } else {
                db.delete_env_var(&profile_id, &key)?;
                self.vars = None;
            }
        }

        // 미리보기 블록은 제거(P3) — 스크린샷은 표 중심. OS override 정보는 각 행
        // dot hover 툴팁으로 제공(정보 손실 없음).
        if !controls_visible {
            // 기본 숨김(P1)이어도 프로파일 관리 진입점은 남긴다 — 작은 weak 링크(codex Med).
            ui.add_space(10.0);
            let link = ui.add(
                egui::Label::new(
                    egui::RichText::new(catalog.t("env.manage_profiles", &[]))
                        .size(12.0)
                        .color(ui.visuals().weak_text_color()),
                )
                .sense(egui::Sense::click()),
            );
            if link.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if link.clicked() {
                self.show_profile_controls = true;
            }
        }

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        Ok(action)
    }
}

fn env_api_section_header(
    ui: &mut egui::Ui,
    title: &str,
    count: Option<usize>,
    action_label: Option<&str>,
) -> bool {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::hover());
    let painter = ui.painter();
    let y = rect.center().y;
    let mut x = rect.left();
    painter.text(
        egui::pos2(x, y),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(14.0),
        ui.visuals().text_color(),
    );
    x += painter
        .layout_no_wrap(
            title.to_owned(),
            egui::FontId::proportional(14.0),
            ui.visuals().text_color(),
        )
        .rect
        .width()
        + 10.0;
    if let Some(count) = count {
        let count_text = count.to_string();
        let count_rect =
            egui::Rect::from_center_size(egui::pos2(x + 14.0, y), egui::vec2(28.0, 28.0));
        painter.rect_filled(count_rect, 0.0, ui.visuals().faint_bg_color);
        painter.text(
            count_rect.center(),
            egui::Align2::CENTER_CENTER,
            count_text,
            egui::FontId::proportional(13.0),
            ui.visuals().hyperlink_color,
        );
    }

    if let Some(action_label) = action_label {
        let button_w = 72.0;
        let button_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - button_w, rect.center().y - 14.0),
            egui::vec2(button_w, 28.0),
        );
        return ui
            .put(button_rect, egui::Button::new(action_label))
            .clicked();
    }
    false
}

fn env_table_header(ui: &mut egui::Ui, columns: &[String]) {
    ui.add_space(2.0);
    env_table_divider(ui);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    let cols = env_table_columns(rect);
    let painter = ui.painter();
    let color = ui.visuals().weak_text_color();
    let font = egui::FontId::proportional(12.0);
    for (idx, column) in columns.iter().take(2).enumerate() {
        painter.text(
            egui::pos2(cols[idx].left() + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            column,
            font.clone(),
            color,
        );
    }
    env_table_divider(ui);
}

struct EnvRowResponse {
    delete: bool,
    toggle_reveal: bool,
}

fn env_table_row(
    ui: &mut egui::Ui,
    var: &EnvVarRow,
    credentials: &[CredentialMeta],
    revealed_value: Option<&str>,
    is_masked: bool,
    catalog: &i18n::Catalog,
) -> EnvRowResponse {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::hover());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }

    let cols = env_table_columns(rect);
    let painter = ui.painter();
    let y = rect.center().y;
    painter.with_clip_rect(cols[0]).text(
        egui::pos2(cols[0].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        &var.key,
        egui::FontId::proportional(14.0),
        ui.visuals().hyperlink_color,
    );
    let value_text = if is_masked {
        // 가림 토글 켜짐 — plain/secret 모두 마스킹(secret은 credential 힌트 형태).
        match &var.value {
            EnvValue::Plain(_) => "••••••••".to_owned(),
            EnvValue::Secret { .. } => display_env_value(
                &var.value,
                credentials,
                &catalog.t("env.deleted_credential", &[]),
            ),
        }
    } else {
        match revealed_value {
            // 기본 노출 — secret은 로드 시 resolve된 평문 캐시(2026-07-09).
            Some(plain) => plain.to_owned(),
            None => display_env_value(
                &var.value,
                credentials,
                &catalog.t("env.deleted_credential", &[]),
            ),
        }
    };
    painter.with_clip_rect(cols[1]).text(
        egui::pos2(cols[1].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        value_text,
        egui::FontId::proportional(14.0),
        ui.visuals().text_color(),
    );

    // dot 토글: **꺼짐(○)=노출(기본)**, 켜짐(● accent)=가림(사용자 2026-07-09).
    // hover 툴팁: secret은 keyring 저장 안내, plain은 OS override 여부.
    let dot_center = egui::pos2(rect.right() - 72.0, y);
    let is_secret = matches!(var.value, EnvValue::Secret { .. });
    let has_os_override = std::env::var_os(&var.key).is_some();
    let (dot_text, dot_color) = if is_masked {
        ("●", ui.visuals().hyperlink_color)
    } else {
        ("○", ui.visuals().weak_text_color())
    };
    painter.text(
        dot_center,
        egui::Align2::CENTER_CENTER,
        dot_text,
        egui::FontId::proportional(12.0),
        dot_color,
    );
    let dot_rect = egui::Rect::from_center_size(dot_center, egui::vec2(20.0, 20.0));
    // secret이면서 OS env에도 같은 키가 있으면 두 정보를 함께(정보 손실 방지 — codex Low).
    let mut hover = String::new();
    if is_secret {
        hover.push_str(&catalog.t("env.secret_stored", &[]));
    }
    if has_os_override {
        if !hover.is_empty() {
            hover.push('\n');
        }
        hover.push_str(&catalog.t("env.os_override", &[]));
    }
    let dot_resp = ui.interact(
        dot_rect,
        ui.id().with(("env_dot", &var.key)),
        egui::Sense::click(),
    );
    let mut toggle_reveal = false;
    if dot_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if dot_resp.clicked() {
        toggle_reveal = true;
    }
    if !hover.is_empty() {
        dot_resp.on_hover_text(hover);
    }

    let delete_rect =
        egui::Rect::from_center_size(egui::pos2(rect.right() - 40.0, y), egui::vec2(22.0, 18.0));
    let delete = ui
        .interact(
            delete_rect,
            ui.id().with(("env_var_delete", &var.key)),
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
        egui::FontId::proportional(11.0),
        text,
    );
    EnvRowResponse {
        delete: delete.clicked(),
        toggle_reveal,
    }
}

fn env_empty_placeholder_row(ui: &mut egui::Ui, catalog: &i18n::Catalog) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 46.0), egui::Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let clicked = response.clicked();
    if clicked {
        ui.memory_mut(|mem| mem.request_focus(env_var_key_input_id()));
    }

    let cols = env_table_columns(rect);
    let y = rect.center().y;
    ui.painter().with_clip_rect(cols[0]).text(
        egui::pos2(cols[0].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        "NEW_VAR",
        egui::FontId::proportional(14.0),
        ui.visuals().hyperlink_color,
    );
    ui.painter().with_clip_rect(cols[1]).text(
        egui::pos2(cols[1].left() + 6.0, y),
        egui::Align2::LEFT_CENTER,
        catalog.t("env.empty_value_placeholder", &[]),
        egui::FontId::proportional(14.0),
        ui.visuals().weak_text_color(),
    );

    let override_center = egui::pos2(rect.right() - 72.0, y);
    ui.painter().text(
        override_center,
        egui::Align2::CENTER_CENTER,
        "○",
        egui::FontId::proportional(12.0),
        ui.visuals().weak_text_color(),
    );
    let delete_rect =
        egui::Rect::from_center_size(egui::pos2(rect.right() - 40.0, y), egui::vec2(22.0, 18.0));
    ui.painter().rect_stroke(
        delete_rect,
        0.0,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        delete_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::proportional(11.0),
        ui.visuals().weak_text_color(),
    );
    clicked
}

fn env_table_columns(rect: egui::Rect) -> [egui::Rect; 2] {
    let action_w = 92.0;
    let content = egui::Rect::from_min_max(
        rect.min,
        egui::pos2((rect.right() - action_w).max(rect.left()), rect.bottom()),
    );
    let key_w = content.width() * 0.45;
    let key = egui::Rect::from_min_size(content.min, egui::vec2(key_w, content.height()));
    let value = egui::Rect::from_min_max(
        egui::pos2(key.right(), content.top()),
        egui::pos2(content.right(), content.bottom()),
    );
    [key, value]
}

fn env_table_divider(ui: &mut egui::Ui) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    let y = ui.painter().round_to_pixel_center(rect.center().y);
    ui.painter()
        .hline(rect.x_range(), y, egui::Stroke::new(1.0, color));
}

fn display_env_value(
    value: &EnvValue,
    credentials: &[CredentialMeta],
    deleted_label: &str,
) -> String {
    match value {
        EnvValue::Plain(v) => v.clone(),
        EnvValue::Secret { credential_id } => credentials
            .iter()
            .find(|c| &c.id == credential_id)
            .map(|c| {
                c.masked_hint
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("••••••••••••")
                    .to_owned()
            })
            .unwrap_or_else(|| format!("[{deleted_label}]")),
    }
}

fn compact_profile_controls(
    ui: &mut egui::Ui,
    state: &mut EnvProfilesUi,
    db: &mut Db,
    workspace_id: &str,
    profiles: &[EnvProfileRow],
    selected_is_production: bool,
    catalog: &i18n::Catalog,
) -> anyhow::Result<()> {
    // 상단 배치(#5): 프로파일 라벨 + 칩 + 삭제, 생산 경고, 생성 폼. 아래에 구분선.
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(catalog.t("env.profile", &[]))
                .size(13.0)
                .weak(),
        );
        let mut delete_profile = None;
        for profile in profiles {
            let label = if profile.is_production {
                format!("{} · {}", profile.name, profile.kind)
            } else {
                profile.name.clone()
            };
            if ui
                .selectable_label(
                    state.selected.as_deref() == Some(profile.id.as_str()),
                    label,
                )
                .clicked()
            {
                state.selected = Some(profile.id.clone());
                state.vars = None;
                // 이전 프로파일 컨텍스트의 추가 폼 draft를 버린다(codex Med).
                state.reset_var_form();
            }
            if ui
                .small_button("×")
                .on_hover_text(catalog.t("action.delete", &[]))
                .clicked()
            {
                delete_profile = Some(profile.id.clone());
            }
        }
        if let Some(id) = delete_profile {
            if let Err(e) = db.delete_env_profile(&id) {
                state.error = Some(format!("{e:#}"));
            } else {
                if state.selected.as_deref() == Some(id.as_str()) {
                    state.selected = None;
                }
                state.profiles = None;
                state.vars = None;
                state.error = None;
            }
        }
    });
    if selected_is_production {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            egui::RichText::new(catalog.t("env.production_warning", &[])).size(13.0),
        );
    }
    compact_profile_form(ui, state, db, workspace_id, catalog)?;
    ui.add_space(8.0);
    env_table_divider(ui);
    Ok(())
}

fn compact_env_var_form(
    ui: &mut egui::Ui,
    state: &mut EnvProfilesUi,
    db: &mut Db,
    profile_id: &str,
    is_dotenv: bool,
    credentials: &[CredentialMeta],
    catalog: &i18n::Catalog,
) -> anyhow::Result<Option<(String, String)>> {
    let mut written: Option<(String, String)> = None;
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.var_key)
                .hint_text(catalog.t("common.key", &[]))
                .id_source(env_var_key_input_id())
                .desired_width(180.0),
        );
        if is_dotenv {
            // dotenv profile은 .env 파일이 진실 — 값을 그대로 입력받아 파일에 쓰고,
            // secret 여부는 동기화가 키/값으로 판정해 keyring 보관한다(7번).
            state.var_is_secret = false;
        } else {
            ui.selectable_value(&mut state.var_is_secret, false, "plain");
            ui.selectable_value(&mut state.var_is_secret, true, "secret");
        }
        if state.var_is_secret {
            let current = state
                .var_credential_id
                .as_ref()
                .and_then(|id| credentials.iter().find(|c| &c.id == id))
                .map(|c| c.label.clone())
                .unwrap_or_else(|| catalog.t("common.select", &[]));
            egui::ComboBox::from_id_salt("var_credential_compact")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for cred in credentials {
                        ui.selectable_value(
                            &mut state.var_credential_id,
                            Some(cred.id.clone()),
                            format!("{} ({})", cred.label, cred.provider),
                        );
                    }
                });
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut state.var_plain_value)
                    .hint_text(catalog.t("common.value", &[]))
                    .desired_width(240.0),
            );
        }
        let key = state.var_key.trim();
        let key_valid = !key.is_empty() && !key.contains('=') && !key.contains('\0');
        let filled = key_valid
            && if state.var_is_secret {
                state.var_credential_id.is_some()
            } else {
                true
            };
        if ui
            .add_enabled(filled, egui::Button::new(catalog.t("env.add_var", &[])))
            .clicked()
        {
            if is_dotenv {
                // dotenv는 **파일이 진실**(codex High/Med 통합, 2026-07-10): DB를 직접
                // 만지지 않고 기록만 반환 — App이 파일에 쓰고 즉시 동기화하면 secret
                // 판정(keyring)·DB upsert를 sync가 일관 처리한다. (직접 Plain upsert는
                // secret-like 키에서 storage 검증에 거부됐다 — codex High.)
                written = Some((key.to_owned(), state.var_plain_value.clone()));
                let id = (profile_id.to_owned(), key.to_owned());
                state.revealed.remove(&id);
                state.masked.remove(&id);
                state.var_key.clear();
                state.var_plain_value.clear();
                state.error = None;
            } else {
                let value = if state.var_is_secret {
                    EnvValue::Secret {
                        credential_id: state.var_credential_id.clone().unwrap_or_default(),
                    }
                } else {
                    EnvValue::Plain(state.var_plain_value.clone())
                };
                match Db::validate_env_var_for_persistence(key, &value)
                    .and_then(|_| db.upsert_env_var(profile_id, key, &value))
                {
                    Ok(()) => {
                        // 같은 키 갱신 시 이전 평문/가림 상태 stale — 함께 제거(codex).
                        let id = (profile_id.to_owned(), key.to_owned());
                        state.revealed.remove(&id);
                        state.masked.remove(&id);
                        state.var_key.clear();
                        state.var_plain_value.clear();
                        state.vars = None;
                        state.error = None;
                    }
                    Err(e) => state.error = Some(format!("{e:#}")),
                }
            }
        }
    });
    Ok(written)
}

fn env_var_key_input_id() -> egui::Id {
    egui::Id::new("env_var_key_input_compact")
}

fn compact_profile_form(
    ui: &mut egui::Ui,
    state: &mut EnvProfilesUi,
    db: &mut Db,
    workspace_id: &str,
    catalog: &i18n::Catalog,
) -> anyhow::Result<()> {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.new_name)
                .hint_text(catalog.t("env.create_profile", &[]))
                .desired_width(180.0),
        );
        for kind in ["local", "staging", "production", "custom"] {
            ui.selectable_value(&mut state.new_kind, kind, kind);
        }
        let name_filled = !state.new_name.trim().is_empty();
        if ui
            .add_enabled(
                name_filled,
                egui::Button::new(catalog.t("env.create_profile", &[])),
            )
            .clicked()
        {
            match db.insert_env_profile(workspace_id, state.new_name.trim(), state.new_kind) {
                Ok(_) => {
                    state.new_name.clear();
                    state.profiles = None;
                    state.error = None;
                }
                Err(e) => state.error = Some(format!("{e:#}")),
            }
        }
    });
    Ok(())
}
