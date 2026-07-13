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

/// 프로젝트 환경(env) 관리 창 — `.env` 파일 단일 진실 (E1, 2026-07-13).
/// 프로젝트 폴더가 지정된 워크스페이스만 변수를 편집할 수 있고, 모든 편집은
/// [`EnvAction::DotenvWrite`]로 `.env`에 기록된다. DB 전용 profile은 더 이상
/// 만들지 않는다 — 남은 레거시 profile 변수는 force 동기화가 .env로 이전한다.
pub struct EnvProfilesUi {
    selected: Option<String>,
    var_key: String,
    var_plain_value: String,
    error: Option<String>,
    /// '+ 추가' 클릭 시에만 인라인 추가 폼을 펼친다(스크린샷: 기본은 표만 — P2).
    show_add_form: bool,
    /// 삭제 확인 대기 (profile_id, key) — ×를 눌러도 바로 지우지 않고 모달로 묻는다.
    /// profile_id를 함께 저장해 확인 중 프로파일 전환 시 다른 프로파일의 같은 key가
    /// 지워지는 것을 막는다(codex High 2026-07-10).
    delete_confirm: Option<(String, String)>,
    profiles: Option<Vec<EnvProfileRow>>,
    vars: Option<Vec<EnvVarRow>>,
    /// 레거시 secret 행의 기본 마스킹을 1회 시딩했는가 (E1 이행기 — 캐시 무효화 시 재시딩).
    legacy_mask_seeded: bool,
    /// 사용자가 ○ 토글로 명시적으로 연 secret 평문 캐시. 참조 화면과 안전한 기본값에
    /// 맞춰 secret은 처음에 마스킹하며, keyring 조회도 reveal 시점에만 요청한다.
    revealed: std::collections::HashMap<(String, String), String>,
    /// dot(●) 토글로 가린 키들 — secret은 로드 시 기본으로 포함한다.
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
        self.legacy_mask_seeded = false;
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
    }

    pub fn new() -> Self {
        Self {
            selected: None,
            var_key: String::new(),
            var_plain_value: String::new(),
            error: None,
            revealed: std::collections::HashMap::new(),
            masked: std::collections::HashSet::new(),
            show_add_form: false,
            delete_confirm: None,
            profiles: None,
            vars: None,
            legacy_mask_seeded: false,
            credentials: None,
            cached_workspace: None,
        }
    }

    pub fn contents_compact(
        &mut self,
        ui: &mut egui::Ui,
        db: &mut Db,
        workspace_id: &str,
        project_root: Option<&std::path::Path>,
        reveal_secret: &mut dyn FnMut(&str) -> Option<String>,
        catalog: &i18n::Catalog,
    ) -> anyhow::Result<Option<EnvAction>> {
        if self.cached_workspace.as_deref() != Some(workspace_id) {
            self.profiles = None;
            self.vars = None;
            self.selected = None;
            self.cached_workspace = Some(workspace_id.to_owned());
            self.legacy_mask_seeded = false;
            // 이전 워크스페이스에서 펼친 추가 폼/입력값이 넘어와 엉뚱한 곳에 저장되지 않게.
            self.reset_var_form();
            // v19(#2)부터 credential 목록이 workspace별(소속+전역) — 이전 워크스페이스
            // 목록이 남아 교차 참조되지 않게 캐시를 버린다(codex High).
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

        // .env 일원화(E1): 편집 대상은 dotenv profile 하나뿐이다. DB 전용 profile은
        // 더 이상 자동 생성하지 않고, 남은 레거시 profile은 read-only로만 보여준다
        // (경로가 지정되면 force 동기화가 .env로 이전 — binjari 사고 재발 방지 ⑤).
        let dotenv_profile_id = profiles
            .iter()
            .find(|p| p.kind == "dotenv")
            .map(|p| p.id.clone());
        let legacy_profiles: Vec<EnvProfileRow> = profiles
            .iter()
            .filter(|p| p.kind != "dotenv")
            .cloned()
            .collect();
        // vars 캐시는 dotenv profile 기준 — profile 생성/삭제(동기화)를 감지해 버린다.
        if self.selected != dotenv_profile_id {
            self.selected = dotenv_profile_id.clone();
            self.vars = None;
        }
        let mut action: Option<EnvAction> = None;

        // ⑤ 프로젝트 폴더 미지정: 변수 입력을 차단하고 폴더 지정을 유도한다 — 이름만 있는
        // 워크스페이스에 변수가 들어가 유령이 되는 경로를 구조적으로 막는다.
        if project_root.is_none() {
            env_api_section_header(ui, &catalog.t("env.env_vars", &[]), None, None);
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(catalog.t("env.no_project_path_note", &[]))
                    .size(13.0)
                    .color(ui.visuals().weak_text_color()),
            );
            ui.add_space(8.0);
            if ui
                .button(catalog.t("env.project_folder.choose", &[]))
                .clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                action = Some(EnvAction::SetProjectPath(dir));
            }
            // 레거시 변수는 숨기지 않는다 — 폴더 지정 시 .env로 이전됨을 안내.
            self.legacy_vars_section(ui, db, &legacy_profiles, reveal_secret, catalog)?;
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            return Ok(action);
        }

        let credentials = match &self.credentials {
            Some(c) => c.clone(),
            None => {
                let c = db.list_credentials_for_workspace(workspace_id)?;
                self.credentials = Some(c.clone());
                c
            }
        };
        // dotenv profile이 아직 없으면(.env 파일 없음) 빈 표 + 추가 폼 — 첫 변수 추가가
        // DotenvWrite로 파일을 만들고, 다음 동기화가 profile을 생성한다.
        let profile_id = dotenv_profile_id.unwrap_or_default();
        let vars = if profile_id.is_empty() {
            Vec::new()
        } else {
            match &self.vars {
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
                    for var in &v {
                        if matches!(var.value, EnvValue::Secret { .. }) {
                            self.masked.insert((profile_id.clone(), var.key.clone()));
                        }
                    }
                    self.vars = Some(v.clone());
                    v
                }
            }
        };
        // secret은 기본 마스킹한다. 사용자가 ○를 눌러 masked에서 빠진 항목만 App의
        // background keyring worker에 요청하고, 결과가 오는 프레임에 평문 cache를 채운다.
        for var in &vars {
            if let EnvValue::Secret { credential_id } = &var.value {
                let id = (profile_id.clone(), var.key.clone());
                if !self.masked.contains(&id)
                    && !self.revealed.contains_key(&id)
                    && let Some(plain) = reveal_secret(credential_id)
                {
                    self.revealed.insert(id, plain);
                }
            }
        }

        // 환경 변수: api-like 분리 없이 **전부 한 표**로(#1/#4). API 키는 App이 별도
        // 자격증명 섹션으로 렌더한다 — 여기서 두 번째 "API Keys" 섹션은 만들지 않는다.
        let add_label = format!("+ {}", catalog.t("action.add", &[]));
        if env_api_section_header(
            ui,
            &catalog.t("env.env_vars", &[]),
            Some(vars.len()),
            Some(&add_label),
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
            // secret은 기본 마스킹, plain은 기본 노출이며 dot 토글로 상태를 바꾼다.
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
        }
        if let Some(key) = toggle_mask {
            let id = (profile_id.clone(), key);
            if !self.masked.remove(&id) {
                self.revealed.remove(&id);
                self.masked.insert(id);
            }
        }
        if vars.is_empty() && env_empty_placeholder_row(ui, catalog) {
            self.show_add_form = true;
        }

        if self.show_add_form
            && let Some(written) = compact_env_var_form(ui, self, &profile_id, catalog)
        {
            // .env가 진실(E1): 추가/수정은 파일에 기록하고 동기화가 DB를 따라 갱신한다.
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
                    ui.label(catalog.t("env.var_delete_confirm.body_dotenv", &[("key", &pending)]));
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
            self.masked.remove(&id); // 같은 이름의 새 변수에 삭제 전 토글 상태가 남지 않게
            self.error = None;
            // 8번(codex Med 반영): DB를 먼저 지우지 않는다 — 파일 라인 제거 후
            // sync의 '사라진 키 제거'가 DB row와 전용 credential/keyring까지 일관
            // 정리한다(먼저 지우면 sync가 credential 회수 기회를 잃음).
            action = Some(EnvAction::DotenvWrite { key, value: None });
        }

        // 레거시(DB 전용) profile 잔여 변수 — force 동기화가 .env로 이전하기 전까지
        // 숨기지 않고 보여준다(keyring resolve 실패로 보류된 키 포함 — 데이터 은닉 방지).
        self.legacy_vars_section(ui, db, &legacy_profiles, reveal_secret, catalog)?;

        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        Ok(action)
    }

    /// 레거시(비-dotenv) profile 변수를 read-only 표로 보여준다 (E1 이행기).
    /// 삭제만 허용(DB 직접) — 추가/수정은 .env 일원화 이후 dotenv 경로만 남는다.
    fn legacy_vars_section(
        &mut self,
        ui: &mut egui::Ui,
        db: &mut Db,
        legacy_profiles: &[EnvProfileRow],
        reveal_secret: &mut dyn FnMut(&str) -> Option<String>,
        catalog: &i18n::Catalog,
    ) -> anyhow::Result<()> {
        let mut rows: Vec<(String, EnvVarRow)> = Vec::new();
        for profile in legacy_profiles {
            for var in db.list_env_vars(&profile.id)? {
                rows.push((profile.id.clone(), var));
            }
        }
        if rows.is_empty() {
            return Ok(());
        }
        let credentials = match &self.credentials {
            Some(c) => c.clone(),
            None => Vec::new(),
        };
        ui.add_space(14.0);
        ui.label(
            egui::RichText::new(catalog.t("env.legacy_pending_note", &[]))
                .size(12.0)
                .color(ui.visuals().warn_fg_color),
        );
        env_table_header(
            ui,
            &[catalog.t("common.key", &[]), catalog.t("common.value", &[])],
        );
        // 처음 보는 legacy secret은 기본 마스킹(메인 표의 로드 시점 시딩과 동일 의미).
        if !self.legacy_mask_seeded {
            for (profile_id, var) in &rows {
                if matches!(var.value, EnvValue::Secret { .. }) {
                    self.masked.insert((profile_id.clone(), var.key.clone()));
                }
            }
            self.legacy_mask_seeded = true;
        }
        let mut delete_row: Option<(String, String)> = None;
        let mut toggle_row: Option<(String, String)> = None;
        for (profile_id, var) in &rows {
            let reveal_id = (profile_id.clone(), var.key.clone());
            // 사용자가 ○로 연(마스킹 해제된) secret만 keyring resolve를 요청한다.
            if let EnvValue::Secret { credential_id } = &var.value
                && !self.masked.contains(&reveal_id)
                && !self.revealed.contains_key(&reveal_id)
                && let Some(plain) = reveal_secret(credential_id)
            {
                self.revealed.insert(reveal_id.clone(), plain);
            }
            let is_masked = self.masked.contains(&reveal_id);
            let revealed_value = if is_masked {
                None
            } else {
                self.revealed.get(&reveal_id).map(String::as_str)
            };
            let row = env_table_row(ui, var, &credentials, revealed_value, is_masked, catalog);
            if row.delete {
                delete_row = Some(reveal_id.clone());
            }
            if row.toggle_reveal {
                toggle_row = Some(reveal_id.clone());
            }
        }
        if let Some(id) = toggle_row
            && !self.masked.remove(&id)
        {
            self.revealed.remove(&id);
            self.masked.insert(id);
        }
        if let Some((profile_id, key)) = delete_row {
            self.revealed.remove(&(profile_id.clone(), key.clone()));
            self.masked.remove(&(profile_id.clone(), key.clone()));
            db.delete_env_var(&profile_id, &key)?;
            if db.list_env_vars(&profile_id)?.is_empty() {
                db.delete_env_profile(&profile_id)?;
            }
            self.profiles = None;
            self.legacy_mask_seeded = false;
        }
        Ok(())
    }
}

fn env_api_section_header(
    ui: &mut egui::Ui,
    title: &str,
    count: Option<usize>,
    action_label: Option<&str>,
) -> bool {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::hover());
    let painter = ui.painter();
    let y = rect.center().y;
    let mut x = rect.left();
    let title_font = egui::FontId::monospace(14.0);
    painter.text(
        egui::pos2(x, y),
        egui::Align2::LEFT_CENTER,
        title,
        title_font.clone(),
        ui.visuals().text_color(),
    );
    x += painter
        .layout_no_wrap(title.to_owned(), title_font, ui.visuals().text_color())
        .rect
        .width()
        + 6.0;
    if let Some(count) = count {
        let count_text = count.to_string();
        let count_rect =
            egui::Rect::from_center_size(egui::pos2(x + 10.0, y), egui::vec2(20.0, 20.0));
        let tag = if ui.visuals().dark_mode {
            egui::Color32::from_rgb(0x2a, 0x3a, 0x44)
        } else {
            egui::Color32::from_rgb(0xd0, 0xe8, 0xf4)
        };
        painter.rect_filled(count_rect, 0.0, tag);
        painter.text(
            count_rect.center(),
            egui::Align2::CENTER_CENTER,
            count_text,
            egui::FontId::monospace(12.0),
            ui.visuals().hyperlink_color,
        );
    }

    let mut clicked = false;
    if let Some(action_label) = action_label {
        let label_font = egui::FontId::monospace(13.0);
        let label_width = painter
            .layout_no_wrap(
                action_label.to_owned(),
                label_font.clone(),
                ui.visuals().weak_text_color(),
            )
            .rect
            .width();
        let button_w = (label_width + 16.0).max(58.0);
        let button_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - button_w, rect.center().y - 13.0),
            egui::vec2(button_w, 26.0),
        );
        let response = ui.interact(
            button_rect,
            ui.id().with(("env_api_section_add", title)),
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
            label_font,
            if hovered {
                egui::Color32::WHITE
            } else {
                ui.visuals().weak_text_color()
            },
        );
        clicked = response.clicked();
    }
    paint_table_hline(ui, rect.bottom());
    clicked
}

fn env_table_header(ui: &mut egui::Ui, columns: &[String]) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), egui::Sense::hover());
    let cols = env_table_columns(rect);
    let painter = ui.painter();
    let color = ui.visuals().weak_text_color();
    let font = egui::FontId::monospace(12.0);
    for (idx, column) in columns.iter().take(2).enumerate() {
        painter.text(
            egui::pos2(cols[idx].left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            column,
            font.clone(),
            color,
        );
    }
    paint_table_hline(ui, rect.bottom());
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
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }

    let cols = env_table_columns(rect);
    let painter = ui.painter();
    let y = rect.center().y;
    painter.with_clip_rect(cols[0]).text(
        egui::pos2(cols[0].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        &var.key,
        egui::FontId::monospace(14.0),
        ui.visuals().hyperlink_color,
    );
    let value_text = if is_masked {
        "••••••••••••••••".to_owned()
    } else {
        match revealed_value {
            // 사용자가 연 secret은 background worker가 resolve한 평문 cache를 표시한다.
            Some(plain) => plain.to_owned(),
            None => display_env_value(
                &var.value,
                credentials,
                &catalog.t("env.deleted_credential", &[]),
            ),
        }
    };
    painter.with_clip_rect(cols[1]).text(
        egui::pos2(cols[1].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        value_text,
        egui::FontId::monospace(14.0),
        ui.visuals().text_color(),
    );

    // dot 토글: ○=노출, ● accent=가림. secret은 안전하게 ●가 기본이다.
    // hover 툴팁: secret은 keyring 저장 안내, plain은 OS override 여부.
    let dot_center = cols[2].center();
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
        egui::FontId::monospace(12.0),
        dot_color,
    );
    let dot_rect = egui::Rect::from_center_size(dot_center, egui::vec2(22.0, 18.0));
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
    if response.hovered() {
        painter.rect_stroke(
            dot_rect,
            0.0,
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
    }

    let delete_rect = egui::Rect::from_center_size(cols[3].center(), egui::vec2(22.0, 18.0));
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
        egui::FontId::monospace(11.0),
        text,
    );
    paint_table_hline(ui, rect.top());
    paint_table_hline(ui, rect.bottom());
    EnvRowResponse {
        delete: delete.clicked(),
        toggle_reveal,
    }
}

fn env_empty_placeholder_row(ui: &mut egui::Ui, catalog: &i18n::Catalog) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::click());
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
        egui::pos2(cols[0].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        "NEW_VAR",
        egui::FontId::monospace(14.0),
        ui.visuals().hyperlink_color,
    );
    ui.painter().with_clip_rect(cols[1]).text(
        egui::pos2(cols[1].left() + 2.0, y),
        egui::Align2::LEFT_CENTER,
        catalog.t("env.empty_value_placeholder", &[]),
        egui::FontId::monospace(14.0),
        ui.visuals().weak_text_color(),
    );

    let override_center = cols[2].center();
    ui.painter().text(
        override_center,
        egui::Align2::CENTER_CENTER,
        "○",
        egui::FontId::monospace(12.0),
        ui.visuals().weak_text_color(),
    );
    let delete_rect = egui::Rect::from_center_size(cols[3].center(), egui::vec2(22.0, 18.0));
    ui.painter().text(
        delete_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::monospace(11.0),
        ui.visuals().weak_text_color(),
    );
    paint_table_hline(ui, rect.top());
    paint_table_hline(ui, rect.bottom());
    clicked
}

fn env_table_columns(rect: egui::Rect) -> [egui::Rect; 4] {
    const GAP: f32 = 4.0;
    const ACTION_W: f32 = 24.0;
    let flexible = ((rect.width() - ACTION_W * 2.0 - GAP * 3.0).max(0.0)) / 2.0;
    let key = egui::Rect::from_min_size(rect.min, egui::vec2(flexible, rect.height()));
    let value = egui::Rect::from_min_size(
        egui::pos2(key.right() + GAP, rect.top()),
        egui::vec2(flexible, rect.height()),
    );
    let mask = egui::Rect::from_min_size(
        egui::pos2(value.right() + GAP, rect.top()),
        egui::vec2(ACTION_W, rect.height()),
    );
    let delete = egui::Rect::from_min_size(
        egui::pos2(mask.right() + GAP, rect.top()),
        egui::vec2(ACTION_W, rect.height()),
    );
    [key, value, mask, delete]
}

fn paint_table_hline(ui: &egui::Ui, y: f32) {
    let color = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let y = ui.painter().round_to_pixel_center(y);
    ui.painter()
        .hline(ui.min_rect().x_range(), y, egui::Stroke::new(1.0, color));
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
            // 참조 화면처럼 접미사도 노출하지 않고 완전히 마스킹한다.
            .map(|_| "••••••••••••••••".to_owned())
            .unwrap_or_else(|| format!("[{deleted_label}]")),
    }
}

/// 변수 추가 폼 — `.env` 단일 모드(E1): 키+값을 그대로 입력받아 파일 기록만 반환한다.
/// secret 여부는 동기화가 키/값으로 판정해 keyring 사본(로그 마스킹용)을 만든다.
fn compact_env_var_form(
    ui: &mut egui::Ui,
    state: &mut EnvProfilesUi,
    profile_id: &str,
    catalog: &i18n::Catalog,
) -> Option<(String, String)> {
    let mut written: Option<(String, String)> = None;
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.var_key)
                .hint_text(catalog.t("common.key", &[]))
                .id_source(env_var_key_input_id())
                .desired_width(180.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut state.var_plain_value)
                .hint_text(catalog.t("common.value", &[]))
                .desired_width(240.0),
        );
        let key = state.var_key.trim();
        let key_valid = !key.is_empty() && !key.contains('=') && !key.contains('\0');
        if ui
            .add_enabled(key_valid, egui::Button::new(catalog.t("env.add_var", &[])))
            .clicked()
        {
            // **파일이 진실**(codex High/Med 통합, 2026-07-10): DB를 직접 만지지 않고
            // 기록만 반환 — App이 파일에 쓰고 즉시 동기화하면 secret 판정(keyring)·
            // DB upsert를 sync가 일관 처리한다.
            written = Some((key.to_owned(), state.var_plain_value.clone()));
            let id = (profile_id.to_owned(), key.to_owned());
            state.revealed.remove(&id);
            state.masked.remove(&id);
            state.var_key.clear();
            state.var_plain_value.clear();
            state.error = None;
        }
    });
    written
}

fn env_var_key_input_id() -> egui::Id {
    egui::Id::new("env_var_key_input_compact")
}

#[cfg(test)]
mod tests {
    use super::env_table_columns;

    #[test]
    fn env_columns는_두_flex열과_두_action열을_정확히_배치한다() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(841.0, 30.0));
        let columns = env_table_columns(rect);
        assert_eq!(columns[0].width(), 390.5);
        assert_eq!(columns[1].width(), 390.5);
        assert_eq!(columns[2].width(), 24.0);
        assert_eq!(columns[3].width(), 24.0);
        assert_eq!(columns[3].right(), rect.right());
    }
}
