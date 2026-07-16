use std::path::PathBuf;

use runtime::{InProcessRuntimeClient, RuntimeCommandSink, RuntimeEventReceiver};

use crate::config::Config;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::storage::Db;
use crate::ui;
use mcp_store::PendingApprovalRow;
use secret::KeyringSecretStore;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ApprovalPendingSignature(Vec<String>);

impl ApprovalPendingSignature {
    fn from_rows(rows: &[PendingApprovalRow]) -> Self {
        Self(rows.iter().map(|row| row.id.clone()).collect())
    }
}

struct ApprovalWatcher {
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct EnvProjectRowsJob {
    generation: u64,
    workspaces: Vec<crate::storage::WorkspaceRow>,
}

struct EnvProjectRowsOutcome {
    generation: u64,
    rows: anyhow::Result<Vec<ui::env_project_list::EnvProjectRow>>,
}

struct EnvProjectRowsWorker {
    tx: std::sync::mpsc::SyncSender<EnvProjectRowsJob>,
    rx: std::sync::mpsc::Receiver<EnvProjectRowsOutcome>,
}

struct EnvSecretRevealJob {
    generation: u64,
    credential_id: String,
}

struct EnvSecretRevealOutcome {
    generation: u64,
    credential_id: String,
    value: anyhow::Result<String>,
}

struct EnvSecretRevealWorker {
    tx: std::sync::mpsc::SyncSender<EnvSecretRevealJob>,
    rx: std::sync::mpsc::Receiver<EnvSecretRevealOutcome>,
}

type DotenvState = (bool, Option<std::time::SystemTime>);

struct DotenvSyncJob {
    generation: u64,
    revision: u64,
    workspace_id: String,
    root: Option<PathBuf>,
    previous_state: Option<DotenvState>,
    force: bool,
}

struct DotenvSyncPayload {
    report: Option<crate::dotenv_sync::DotenvSyncReport>,
    env_plain: Vec<(String, String)>,
    env_secrets: Vec<(String, String)>,
}

struct DotenvSyncOutcome {
    generation: u64,
    revision: u64,
    workspace_id: String,
    root: Option<PathBuf>,
    baseline: DotenvState,
    result: anyhow::Result<Option<DotenvSyncPayload>>,
}

struct DotenvSyncWorker {
    tx: std::sync::mpsc::SyncSender<DotenvSyncJob>,
    rx: std::sync::mpsc::Receiver<DotenvSyncOutcome>,
}

fn dotenv_state_for_root(root: Option<&std::path::Path>) -> DotenvState {
    let Some(root) = root else {
        return (false, None);
    };
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    let mut exists = false;
    for name in crate::dotenv_sync::DOTENV_FILE_NAMES {
        match std::fs::metadata(root.join(name)) {
            Ok(meta) => {
                exists = true;
                true.hash(&mut hasher);
                meta.modified()
                    .ok()
                    .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos())
                    .hash(&mut hasher);
            }
            Err(_) => false.hash(&mut hasher),
        }
    }
    let digest = hasher.finish();
    let surrogate = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(digest >> 1);
    (exists, exists.then_some(surrogate))
}

fn load_dotenv_default_env(db: &Db, workspace_id: &str) -> anyhow::Result<(EnvPairs, EnvPairs)> {
    let (mut env_plain, mut env_secrets) = (Vec::new(), Vec::new());
    if let Some(profile) = db
        .list_env_profiles(workspace_id)?
        .into_iter()
        .find(|profile| profile.kind == crate::dotenv_sync::DOTENV_PROFILE_KIND)
    {
        for var in db.list_env_vars(&profile.id)? {
            match var.value {
                crate::env::EnvValue::Plain(value) => env_plain.push((var.key, value)),
                crate::env::EnvValue::Secret { credential_id } => {
                    env_secrets.push((var.key, credential_id));
                }
            }
        }
    }
    Ok((env_plain, env_secrets))
}

impl DotenvSyncWorker {
    fn spawn(db_path: PathBuf, redaction: secret::RedactionService, ctx: egui::Context) -> Self {
        // App은 한 번에 하나만 제출하고 추가 요청은 최신 1건으로 축약한다. 채널도 hard cap을
        // 둬 느린 외장/네트워크 볼륨이나 keychain이 UI 메모리 증가로 번지지 않게 한다.
        let (tx, jobs) = std::sync::mpsc::sync_channel::<DotenvSyncJob>(1);
        let (results, rx) = std::sync::mpsc::sync_channel::<DotenvSyncOutcome>(1);
        std::thread::Builder::new()
            .name("dotenv-sync".to_owned())
            .spawn(move || {
                // 연결은 최초 필요 시 한 번 열어 재사용한다 (EnvProjectRowsWorker 관례) —
                // job마다 열면 PRAGMA/마이그레이션 버전 점검이 매번 반복된다. 열기 실패는
                // 캐시하지 않아 다음 job이 재시도한다.
                let mut cached_db: Option<Db> = None;
                while let Ok(job) = jobs.recv() {
                    // 기준점은 파일을 읽기 직전에 worker에서 캡처한다. 읽는 도중 파일이 다시
                    // 바뀌면 이 옛 기준점과 다음 점검이 달라져 안전하게 한 번 더 동기화된다.
                    let baseline = dotenv_state_for_root(job.root.as_deref());
                    let result = if !job.force && job.previous_state == Some(baseline) {
                        Ok(None)
                    } else if let Some(root) = job.root.as_deref() {
                        match &mut cached_db {
                            Some(db) => Ok(db),
                            slot @ None => Db::open(&db_path).map(|db| slot.insert(db)),
                        }
                        .and_then(|db| {
                            // force 동기화(시작·경로 지정·수동 리프레시)에서만 레거시
                            // DB-전용 profile 변수를 .env로 이전한다(.env 일원화 E1).
                            // 2초 주기 폴링의 fast-path에는 DB 조회를 더하지 않는다.
                            if job.force
                                && let Err(e) =
                                    crate::dotenv_sync::migrate_legacy_profiles_to_dotenv(
                                        db,
                                        &KeyringSecretStore,
                                        &job.workspace_id,
                                        root,
                                    )
                            {
                                tracing::warn!("레거시 env profile 이전 실패: {e:#}");
                            }
                            crate::dotenv_sync::sync_workspace_dotenv(
                                &*db,
                                &KeyringSecretStore,
                                &redaction,
                                &job.workspace_id,
                                root,
                            )
                            .and_then(|report| {
                                let (env_plain, env_secrets) = if report.is_some() {
                                    load_dotenv_default_env(&*db, &job.workspace_id)?
                                } else {
                                    (Vec::new(), Vec::new())
                                };
                                Ok(Some(DotenvSyncPayload {
                                    report,
                                    env_plain,
                                    env_secrets,
                                }))
                            })
                        })
                    } else {
                        Ok(Some(DotenvSyncPayload {
                            report: None,
                            env_plain: Vec::new(),
                            env_secrets: Vec::new(),
                        }))
                    };
                    if results
                        .send(DotenvSyncOutcome {
                            generation: job.generation,
                            revision: job.revision,
                            workspace_id: job.workspace_id,
                            root: job.root,
                            baseline,
                            result,
                        })
                        .is_err()
                    {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("dotenv sync worker thread spawn");
        Self { tx, rx }
    }

    fn try_request(
        &self,
        job: DotenvSyncJob,
    ) -> Result<(), std::sync::mpsc::TrySendError<DotenvSyncJob>> {
        self.tx.try_send(job)
    }
}

impl EnvSecretRevealWorker {
    fn spawn(ctx: egui::Context) -> Self {
        let (tx, jobs) = std::sync::mpsc::sync_channel::<EnvSecretRevealJob>(64);
        let (results, rx) = std::sync::mpsc::sync_channel::<EnvSecretRevealOutcome>(64);
        std::thread::Builder::new()
            .name("env-secret-reveal".to_owned())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let value =
                        secret::SecretStore::get_secret(&KeyringSecretStore, &job.credential_id)
                            .map(|secret| secret.expose().to_owned());
                    if results
                        .send(EnvSecretRevealOutcome {
                            generation: job.generation,
                            credential_id: job.credential_id,
                            value,
                        })
                        .is_err()
                    {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("환경 secret reveal worker thread spawn");
        Self { tx, rx }
    }

    fn try_request(&self, job: EnvSecretRevealJob) -> bool {
        self.tx.try_send(job).is_ok()
    }
}

impl EnvProjectRowsWorker {
    fn spawn(db_path: PathBuf, ctx: egui::Context) -> Self {
        let (tx, jobs) = std::sync::mpsc::sync_channel::<EnvProjectRowsJob>(1);
        let (results, rx) = std::sync::mpsc::sync_channel::<EnvProjectRowsOutcome>(1);
        std::thread::Builder::new()
            .name("env-project-rows".to_owned())
            .spawn(move || {
                let mut db = None;
                while let Ok(job) = jobs.recv() {
                    let rows = (|| -> anyhow::Result<_> {
                        if db.is_none() {
                            db = Some(Db::open(&db_path)?);
                        }
                        let db = db.as_ref().expect("DB initialized above");
                        db.env_api_project_counts().map(|counts| {
                            let counts: std::collections::HashMap<_, _> = counts
                                .into_iter()
                                .map(|count| (count.workspace_id.clone(), count))
                                .collect();
                            job.workspaces
                                .iter()
                                .map(|workspace| {
                                    let count = counts.get(&workspace.id);
                                    let path = workspace.path.clone();
                                    ui::env_project_list::EnvProjectRow {
                                        id: workspace.id.clone(),
                                        name: App::workspace_display_name(workspace),
                                        alias: workspace.name.clone(),
                                        path_missing: !path.trim().is_empty()
                                            && !std::path::Path::new(&path).is_dir(),
                                        path,
                                        env_count: count.map_or(0, |count| count.env_count),
                                        key_count: count.map_or(0, |count| count.key_count),
                                    }
                                })
                                .collect()
                        })
                    })();
                    if results
                        .send(EnvProjectRowsOutcome {
                            generation: job.generation,
                            rows,
                        })
                        .is_err()
                    {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("환경 프로젝트 worker thread spawn");
        Self { tx, rx }
    }

    fn try_request(
        &self,
        job: EnvProjectRowsJob,
    ) -> Result<(), std::sync::mpsc::TrySendError<EnvProjectRowsJob>> {
        self.tx.try_send(job)
    }
}

impl ApprovalWatcher {
    fn spawn(
        db_path: PathBuf,
        ctx: egui::Context,
        poll_requested: Arc<AtomicBool>,
        interval: std::time::Duration,
    ) -> Self {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("approval-watcher".to_owned())
            .spawn(move || {
                let db = match Db::open(&db_path) {
                    Ok(db) => db,
                    Err(e) => {
                        tracing::warn!("approval watcher DB 열기 실패: {e:#}");
                        return;
                    }
                };
                let mut last = ApprovalPendingSignature::default();
                loop {
                    match stop_rx.recv_timeout(interval) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    match db.list_pending_approvals() {
                        Ok(rows) => {
                            let next = ApprovalPendingSignature::from_rows(&rows);
                            if next != last {
                                last = next;
                                poll_requested.store(true, Ordering::Release);
                                ctx.request_repaint();
                            }
                        }
                        Err(e) => tracing::warn!("approval watcher 조회 실패: {e:#}"),
                    }
                }
            })
            .expect("approval watcher thread spawn");
        Self {
            stop_tx: Some(stop_tx),
            handle: Some(handle),
        }
    }

    fn stop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ApprovalWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

struct AppCredentialService<'a> {
    db: &'a Db,
    secret_store: &'a dyn secret::SecretStore,
    redaction: &'a secret::RedactionService,
    /// 현재 workspace — 목록은 소속+전역, 새 credential은 이 workspace 소속(#2, v19).
    workspace_id: &'a str,
}

impl ui::credentials::CredentialService for AppCredentialService<'_> {
    fn list_credentials(&self) -> anyhow::Result<Vec<ui::credentials::CredentialListItem>> {
        // .env 자동 동기화로 생긴(=env var가 참조하는) credential은 환경 변수 표에 이미
        // 마스킹으로 나온다 — 'API 키' 표에 또 나오면 같은 데이터가 두 번 세어져 혼란
        // (사용자 2026-07-09: 23env/23key 중복). 수동 등록/커넥터용만 남긴다.
        let referenced = self.db.env_referenced_credential_ids()?;
        Ok(self
            .db
            .list_credentials_for_workspace(self.workspace_id)?
            .into_iter()
            .filter(|meta| !referenced.contains(&meta.id))
            .map(|meta| ui::credentials::CredentialListItem {
                id: meta.id,
                provider: meta.provider,
                label: meta.label,
                credential_kind: meta.credential_kind,
                masked_hint: meta.masked_hint,
            })
            .collect())
    }

    fn add_credential(&self, credential: ui::credentials::NewCredential) -> anyhow::Result<()> {
        let secret = secret::SecretString::new(credential.secret);
        // 새 credential은 즉시 로그 redaction 대상이다. JSON service account 형태도
        // 필드 단위로 등록해 후속 session/MCP output에서 마스킹된다.
        self.redaction.register(&secret);
        self.redaction.register_json_fields(&secret);
        let id = uuid::Uuid::new_v4().to_string();
        self.secret_store.set_secret(&id, &secret)?;
        let meta = crate::storage::CredentialMeta {
            id: id.clone(),
            provider: credential.provider,
            label: credential.label,
            credential_kind: credential.credential_kind,
            masked_hint: Some(secret::masked_hint(secret.expose())),
            // 환경 UI에서 추가한 키는 현재 프로젝트 소속(#2).
            workspace_id: Some(self.workspace_id.to_owned()),
        };
        if let Err(e) = self.db.insert_credential(&meta) {
            if let Err(rollback) = self.secret_store.delete_secret(&id) {
                tracing::warn!(credential_id = %id, "rollback 실패 — 고아 keyring entry: {rollback:#}");
            }
            return Err(e);
        }
        tracing::info!(credential_id = %id, "credential 추가");
        Ok(())
    }

    fn delete_credential(&self, id: &str) -> anyhow::Result<()> {
        // 순서 근거:
        // 1) 참조 검사 — 참조 중이면 아무것도 건드리지 않는다.
        // 2) keyring 삭제 먼저 — 실패하면 metadata가 남아 사용자가 재시도할 수 있다.
        // 3) 조건부 DB 삭제 — 참조 race가 생기면 행이 남고, secret은 이미 지워진다.
        if self.db.credential_in_use(id)? {
            anyhow::bail!("env var가 참조 중인 credential입니다 — 해당 변수를 먼저 삭제하세요");
        }
        self.secret_store.delete_secret(id)?;
        self.secret_store
            .delete_secret(&auth::refresh_entry_id(id))?;
        // DCR client_secret entry도 함께 정리 (H4 리뷰 P2 — `{id}.dcr` 고아 방지)
        self.secret_store
            .delete_secret(&auth::dcr_secret_entry_id(id))?;
        if !self.db.delete_credential_if_unused(id)? {
            anyhow::bail!(
                "삭제 중 env var 참조가 생겼습니다 — secret은 지워졌으니 변수 정리 후 다시 삭제하세요"
            );
        }
        tracing::info!(credential_id = %id, "credential 삭제");
        Ok(())
    }

    fn orphan_credentials(&self) -> anyhow::Result<Vec<String>> {
        #[cfg(target_os = "macos")]
        {
            let mut known: std::collections::HashSet<String> = self
                .db
                .list_credentials()?
                .into_iter()
                .map(|c| c.id)
                .collect();
            // MCP env 참조도 live — metadata 없이 keyring만 있는 사용 중 secret 보호
            // (codex Med).
            known.extend(self.db.mcp_referenced_credential_ids()?);
            let mut orphans: Vec<String> = scan_keychain_accounts()?
                .into_iter()
                .filter(|acct| {
                    // UUID(또는 uuid.refresh)만 후보 — base가 DB에 없으면 고아.
                    uuid_base(acct).is_some_and(|base| !known.contains(base))
                })
                .collect();
            orphans.sort();
            orphans.dedup();
            Ok(orphans)
        }
        #[cfg(not(target_os = "macos"))]
        Ok(Vec::new())
    }

    fn purge_orphan_credentials(&self, ids: &[String]) -> anyhow::Result<usize> {
        let mut purged = 0usize;
        for id in ids {
            // keyring API 대신 security CLI로 삭제(2026-07-10): 고아는 구 서명 시절
            // 생성이라 partition 불일치로 keyring 접근마다 키체인 암호를 물었다
            // ('항상 허용'도 유지 안 됨). CLI 삭제는 이 검사를 거치지 않아 무프롬프트.
            #[cfg(target_os = "macos")]
            let ok = std::process::Command::new("/usr/bin/security")
                .args([
                    "delete-generic-password",
                    "-s",
                    secret::KEYRING_SERVICE,
                    "-a",
                    id,
                ])
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false);
            #[cfg(not(target_os = "macos"))]
            let ok = self.secret_store.delete_secret(id).is_ok();
            if ok {
                purged += 1;
            } else {
                tracing::warn!(account = %id, "고아 keyring 삭제 실패");
            }
        }
        tracing::info!(purged, "고아 keyring 항목 정리");
        Ok(purged)
    }
}

#[derive(Default)]
struct EnvApiProjectEditState {
    name_workspace_id: Option<String>,
    name_buffer: String,
}

/// 환경/API 상세 상단 헤더 — 참조 화면의 68px 고정 헤더와 14px 좌우 inset.
/// 이름/경로는 클릭해 인라인 편집하며, 화면에 없는 폴더 관리 동작은 경로 우클릭 메뉴에
/// 보존한다. 따라서 표준 상태의 픽셀 배치는 목업과 같고 기존 기능도 잃지 않는다.
fn render_env_api_project_header(
    ui: &mut egui::Ui,
    project: Option<&ui::env_project_list::EnvProjectRow>,
    env_action: &mut Option<ui::env_profiles::EnvAction>,
    workspace_rename: &mut Option<String>,
    edit: &mut EnvApiProjectEditState,
    catalog: &i18n::Catalog,
) {
    let project_id = project.map(|project| project.id.as_str()).unwrap_or("");
    let name = project
        .map(|project| project.name.as_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("~");
    // 이름 편집 초기값은 표시명("폴더명 (별칭)")이 아니라 별칭 원본이다 (E3).
    let alias = project.map(|project| project.alias.trim()).unwrap_or("");
    let path = project
        .map(|project| project.path.as_str())
        .filter(|path| !path.trim().is_empty())
        .unwrap_or("");
    // 프로젝트 목록을 만들 때 계산한 값을 재사용한다. Path::is_dir()는 네트워크/외장
    // 볼륨에서 블록될 수 있으므로 설정 UI의 매 프레임 렌더 경로에서 다시 호출하지 않는다.
    let path_missing = project.is_some_and(|project| project.path_missing);
    let path_text = if path.is_empty() {
        catalog.t("workspace.manager.path_unset", &[])
    } else {
        ui::env_project_list::display_project_path(path)
    };

    const HEADER_H: f32 = 68.0;
    const PAD_X: f32 = 14.0;
    const LABEL_W: f32 = 34.0;
    const LABEL_GAP: f32 = 6.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), HEADER_H),
        egui::Sense::hover(),
    );
    let painter = ui.painter().clone();
    let value_x = rect.left() + PAD_X + LABEL_W + LABEL_GAP;
    let name_y = rect.top() + 21.0;
    let path_y = rect.top() + 49.0;
    let value_right = rect.right() - PAD_X;

    painter.text(
        egui::pos2(rect.left() + PAD_X, name_y),
        egui::Align2::LEFT_CENTER,
        catalog.t("common.name", &[]),
        egui::FontId::monospace(13.0),
        ui.visuals().weak_text_color(),
    );
    painter.text(
        egui::pos2(rect.left() + PAD_X, path_y),
        egui::Align2::LEFT_CENTER,
        catalog.t("workspace.manager.path", &[]),
        egui::FontId::monospace(13.0),
        ui.visuals().weak_text_color(),
    );

    let name_rect = egui::Rect::from_min_max(
        egui::pos2(value_x, rect.top() + 8.0),
        egui::pos2(value_right, rect.top() + 34.0),
    );
    if edit.name_workspace_id.as_deref() == Some(project_id) {
        let response = ui.put(
            name_rect,
            egui::TextEdit::singleline(&mut edit.name_buffer)
                .font(egui::TextStyle::Monospace)
                .id_source(("env_api_project_name", project_id)),
        );
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let commit = response.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter));
        if escape && response.has_focus() {
            edit.name_workspace_id = None;
            edit.name_buffer.clear();
        } else if commit {
            // E3: 편집 대상은 표시명이 아니라 **별칭**이다. 빈 값 = 별칭 해제(폴더명만 표시).
            let next = edit.name_buffer.trim();
            if next != alias {
                *workspace_rename = Some(next.to_owned());
            }
            edit.name_workspace_id = None;
            edit.name_buffer.clear();
        }
    } else {
        painter.with_clip_rect(name_rect).text(
            egui::pos2(value_x, name_y),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::monospace(15.0),
            ui.visuals().text_color(),
        );
        let response = ui
            .interact(
                name_rect,
                ui.id().with(("env_api_project_name_label", project_id)),
                egui::Sense::click(),
            )
            .on_hover_text(catalog.t("workspace.alias_hint", &[]));
        if response.clicked() && !project_id.is_empty() {
            edit.name_workspace_id = Some(project_id.to_owned());
            edit.name_buffer = alias.to_owned();
        }
    }

    let path_rect = egui::Rect::from_min_max(
        egui::pos2(value_x, rect.top() + 36.0),
        egui::pos2(value_right, rect.top() + 62.0),
    );
    {
        let path_color = if path_missing {
            ui.visuals().error_fg_color
        } else {
            ui.visuals().weak_text_color()
        };
        painter.with_clip_rect(path_rect).text(
            egui::pos2(value_x, path_y),
            egui::Align2::LEFT_CENTER,
            &path_text,
            egui::FontId::monospace(14.0),
            path_color,
        );
        let hover_text = if path_missing {
            format!(
                "{}\n{}",
                path_text,
                catalog.t("env.project_path_missing", &[])
            )
        } else {
            path_text.clone()
        };
        let response = ui
            .interact(
                path_rect,
                ui.id().with(("env_api_project_path_label", project_id)),
                egui::Sense::click(),
            )
            .on_hover_text(hover_text);
        // E3 ④: 경로는 타이핑이 아니라 Finder로만 지정한다 — 오타/존재하지 않는 경로로
        // 워크스페이스가 유령이 되는 입력 경로 제거. 클릭 = 폴더 선택 다이얼로그.
        if response.clicked()
            && !project_id.is_empty()
            && let Some(dir) = rfd::FileDialog::new().pick_folder()
        {
            *env_action = Some(ui::env_profiles::EnvAction::SetProjectPath(dir));
        }
        response.context_menu(|ui| {
            if ui
                .button(catalog.t("env.project_folder.choose", &[]))
                .clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                *env_action = Some(ui::env_profiles::EnvAction::SetProjectPath(dir));
                ui.close();
            }
            if !path.is_empty()
                && ui
                    .button(catalog.t("env.project_folder.clear", &[]))
                    .clicked()
            {
                *env_action = Some(ui::env_profiles::EnvAction::SetProjectPath(
                    std::path::PathBuf::new(),
                ));
                ui.close();
            }
            if !path.is_empty() && ui.button(catalog.t("env.resync_hint", &[])).clicked() {
                *env_action = Some(ui::env_profiles::EnvAction::Resync);
                ui.close();
            }
        });
    }

    let y = painter.round_to_pixel_center(rect.bottom());
    painter.hline(
        rect.x_range(),
        y,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
}

/// keyring 고아 항목 스캔/정리 (macOS security CLI) — 삭제된 env profile/credential이
/// 남긴 keyring 잔여(실측 48개, 2026-07-08) 청소. **UUID 형태의 계정만** 후보로 삼아
/// 앱의 시스템 키(remote-tls-* 등)는 건드리지 않는다.
#[cfg(target_os = "macos")]
fn scan_keychain_accounts() -> anyhow::Result<Vec<String>> {
    let out = std::process::Command::new("/usr/bin/security")
        .arg("dump-keychain")
        .output()?;
    anyhow::ensure!(out.status.success(), "security dump-keychain 실패");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut accounts = Vec::new();
    let mut acct: Option<String> = None;
    let mut svce_match = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("class:") {
            if svce_match && let Some(a) = acct.take() {
                accounts.push(a);
            }
            acct = None;
            svce_match = false;
        } else if let Some(rest) = line.strip_prefix("\"acct\"<blob>=\"") {
            acct = rest.strip_suffix('\"').map(str::to_owned);
        } else if line.contains("\"svce\"<blob>=\"") && line.contains(secret::KEYRING_SERVICE) {
            svce_match = true;
        }
    }
    if svce_match && let Some(a) = acct.take() {
        accounts.push(a);
    }
    Ok(accounts)
}

/// UUID v4 형태(8-4-4-4-12 hex)인가 — credential id 규약. `.refresh`/`.dcr`
/// 접미(OAuth refresh token / DCR client_secret entry — H4 규약)는 벗겨 판정.
fn uuid_base(account: &str) -> Option<&str> {
    let base = account
        .strip_suffix(".refresh")
        .or_else(|| account.strip_suffix(".dcr"))
        .unwrap_or(account);
    let bytes = base.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    for (i, b) in bytes.iter().enumerate() {
        let ok = match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        };
        if !ok {
            return None;
        }
    }
    Some(base)
}

struct AppOAuthCredentialStore<'a> {
    secret_store: &'a dyn secret::SecretStore,
    redaction: &'a secret::RedactionService,
}

impl ui::connectors::OAuthCredentialStore for AppOAuthCredentialStore<'_> {
    fn store_oauth_token(
        &self,
        token: &auth::OAuthToken,
    ) -> anyhow::Result<ui::connectors::StoredOAuthCredential> {
        let id = uuid::Uuid::new_v4().to_string();
        self.update_oauth_token(&id, token)?;
        Ok(ui::connectors::StoredOAuthCredential {
            id,
            masked_hint: secret::masked_hint(token.access_token.expose()),
        })
    }

    /// 재승인(H5) — 기존 credential id 아래 토큰 재저장 (env 참조 유지).
    fn update_oauth_token(&self, id: &str, token: &auth::OAuthToken) -> anyhow::Result<()> {
        auth::store_token(self.secret_store, id, token)?;
        self.redaction.register(&token.access_token);
        if let Some(refresh) = &token.refresh_token {
            self.redaction.register(refresh);
        }
        Ok(())
    }

    /// DCR client_secret keyring 관리 (H5) — Some이면 `{id}.dcr` 저장, None이면 정리.
    fn set_dcr_secret(
        &self,
        id: &str,
        secret: Option<&secret::SecretString>,
    ) -> anyhow::Result<()> {
        let entry = auth::dcr_secret_entry_id(id);
        match secret {
            Some(secret) => {
                self.secret_store.set_secret(&entry, secret)?;
                self.redaction.register(secret);
            }
            None => {
                // entry가 없어도 정리 성공으로 취급 (재등록으로 secret이 사라진 경우)
                if let Err(e) = self.secret_store.delete_secret(&entry) {
                    tracing::debug!("DCR secret entry 정리 생략: {e:#}");
                }
            }
        }
        Ok(())
    }

    fn delete_oauth_token(&self, id: &str) -> anyhow::Result<()> {
        self.secret_store.delete_secret(id)?;
        self.secret_store
            .delete_secret(&auth::refresh_entry_id(id))?;
        // DCR client_secret entry도 함께 정리 (H4 리뷰 P2)
        self.secret_store
            .delete_secret(&auth::dcr_secret_entry_id(id))?;
        Ok(())
    }
}

struct AppMcpScopedEnvResolver<'a> {
    secret_store: &'a dyn secret::SecretStore,
    redaction: &'a secret::RedactionService,
}

impl ui::connectors::McpScopedEnvResolver for AppMcpScopedEnvResolver<'_> {
    fn resolve_mcp_env(
        &self,
        env_plain: &[(String, String)],
        env_secrets: &[(String, String)],
    ) -> anyhow::Result<Vec<(String, String)>> {
        mcp_store::validate_server_env_for_persistence(env_plain, env_secrets)?;
        let mut env = env_plain.to_vec();
        for (key, credential_id) in env_secrets {
            let secret = self
                .secret_store
                .get_secret(credential_id)
                .map_err(|e| anyhow::anyhow!("MCP env '{}' credential 조회 실패: {e:#}", key))?;
            self.redaction.register(&secret);
            self.redaction.register_json_fields(&secret);
            env.push((key.clone(), secret.expose().to_owned()));
        }
        Ok(env)
    }
}

/// 한 workspace의 런타임 상태 묶음 (워커-per-workspace §14.1 준비 — Stage A).
/// 활성 workspace는 렌더되고, (후속) warm workspace는 이벤트만 드레인된다.
struct WorkspaceRuntime {
    id: String,
    runtime: InProcessRuntimeClient,
    events: RuntimeEventReceiver,
    workspace_ui: ui::workspace::WorkspaceUi,
    /// worker에 마지막으로 보낸 render 활성 상태 (§14.1 Active↔Warm) — 전이 시에만 전송
    render_active: bool,
    /// logic()에서 drain했지만 아직 ui()가 렌더에 소비하지 않은 이벤트 (§14.1 Warm:
    /// 알림은 logic()에서 처리하고 렌더는 Active 복귀 시 ui()가 몰아서 소비).
    pending_events: Vec<runtime::RuntimeEvent>,
    /// 세션→제목 캐시 (MuxUpdated에서 누적) — Warm 동안 mux가 안 갱신돼도 알림 제목을
    /// 해석하기 위함. exit 처리 후 제거해 live 세션으로 유계.
    /// 세션별 **raw** pane 제목("workspace.spawn.shell 3"). 표시 시점에 해석한다 —
    /// 활동 패널/폰은 프로젝트명 규칙(activity_session_name), 알림은 i18n 렌더.
    session_titles: std::collections::HashMap<runtime::SessionId, String>,
    /// 마지막 worker resource sample. PR-U25 activity view 표시용.
    resource_usage: Option<runtime::ProcessResourceSnapshot>,
    /// 마지막 worker child-process resource samples. Runtime이 집계한 값만 보관한다.
    session_resource_usage: Vec<runtime::SessionResourceUsage>,
    /// 마지막 PTY input pressure signal(+관측 시각). 회복 이벤트가 없어(QueueFull은
    /// writer drain으로 조용히 해소) 표시 시 TTL로 stale 뱃지를 걸러낸다(codex 2026-07-08).
    input_pressure: Option<(runtime::PtyInputPressure, std::time::Instant)>,
    /// 세션별 마지막 input pressure(+관측 시각) — 활동 뷰 pane 서브행용. exit 시 제거.
    session_input_pressure: std::collections::HashMap<
        runtime::SessionId,
        (runtime::PtyInputPressure, std::time::Instant),
    >,
    /// Warm으로 내려간 시각. 일정 시간 이후 자동 Suspended(워커 shutdown)로 내린다.
    backgrounded_at: Option<std::time::Instant>,
    /// live 세션 추적 (suspend 보호 — 이벤트 스트림에서 갱신).
    live: LiveSessionTracker,
    /// 워커 생성 시각 — 첫 MuxUpdated 관측 전 suspend 유예(RESTORE 관측 창) 판정용.
    created: std::time::Instant,
    /// 응답(AgentSpawned/SpawnFailed) 대기 중인 agent spawn 수 — 전환 시 전역
    /// AgentsUi에서 이관받는다 (agent spawn 직후 전환 race의 live 판정).
    pending_agent_spawns: u32,
    /// 새 runtime은 background dotenv 결과를 먼저 적용한 뒤 RestoreWorkspace를 보낸다.
    /// 느린 볼륨/keychain 때문에 복원이 영원히 막히지 않도록 logic에서 timeout fallback한다.
    restore_pending_since: Option<std::time::Instant>,
    /// durable 구독 overflow 뒤 이미 큐에 들어온 이벤트를 budget 단위로 끝까지 적용한 다음
    /// fresh receiver로 재구독하기 위한 상태.
    event_overflow_pending: bool,
    /// active receiver 재구독 뒤 전체 mux/viewport snapshot 재전송 명령이 아직 남아 있다.
    event_resync_pending: bool,
}

impl WorkspaceRuntime {
    /// 아직 종료(Exited)되지 않은 세션이 pane에 하나라도 있으면 true — 셸이든
    /// 에이전트든 떠 있는 것 자체가 실행 중이다. 이런 workspace는 Suspended(워커
    /// shutdown = PTY kill)로 내리면 안 된다 (2026-07-05 사용자 요구: 진행 중인
    /// 에이전트 작업이 경고 없이 죽는 문제).
    ///
    /// tracker 외 두 가지를 추가로 live 취급한다 (codex High — spawn/restore race):
    /// - 응답 대기 중인 셸 spawn (명령이 큐/워커에 있고 MuxUpdated가 아직 안 옴)
    /// - 워커 생성 직후 첫 MuxUpdated 관측 전의 유예 창 (RestoreWorkspace 복원 세션이
    ///   아직 이벤트로 안 왔을 수 있다 — 빈 workspace는 restore가 emit하지 않으므로
    ///   유예가 끝나면 정상적으로 suspend 가능해진다)
    fn has_live_sessions(&self) -> bool {
        workspace_is_live(
            self.live.has_live(),
            self.live.seen_mux,
            self.workspace_ui.pending_spawns() + self.pending_agent_spawns,
            self.created.elapsed(),
        )
    }

    /// 30분 warm timeout 뒤 안전하게 재생성 가능한 "프롬프트 대기 셸만" 남았는지.
    /// 에이전트/미분류 세션, 자식 프로세스, resource 샘플 부재는 모두 작업 중으로 보고
    /// 보호한다. 셸 자체는 layout/cwd에서 다시 spawn되므로 이 조건에서만 suspend 가능하다.
    fn can_auto_suspend_idle_shells(&self) -> bool {
        if self.workspace_ui.pending_spawns() + self.pending_agent_spawns > 0 || !self.live.seen_mux
        {
            return false;
        }
        let Some(sessions) = self.live.live_shell_sessions() else {
            return false;
        };
        shell_sessions_are_idle(&sessions, &self.session_resource_usage, |session| {
            self.workspace_ui.agent_line_for(session).is_some()
        })
    }
}

/// suspend 보호의 live 판정 (순수 함수 — 테스트 용이).
fn workspace_is_live(
    tracker_live: bool,
    seen_mux: bool,
    pending_spawns: u32,
    age: std::time::Duration,
) -> bool {
    /// 첫 MuxUpdated 관측 전 suspend를 미루는 유예 — restore 이벤트 전파(수 ms)보다
    /// 넉넉히. 빈 workspace는 이 유예만 지나면 suspend 대상이 된다.
    const RESTORE_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
    tracker_live || pending_spawns > 0 || (!seen_mux && age < RESTORE_GRACE)
}

fn projected_live_warm_count(
    current_live_warm: usize,
    target_is_live_warm: bool,
    active_will_be_live: bool,
) -> usize {
    current_live_warm.saturating_sub(usize::from(target_is_live_warm))
        + usize::from(active_will_be_live)
}

/// 이벤트 스트림에서 "pane에 붙어 있고 아직 Exited 안 된 세션"을 추적한다.
/// MuxUpdated가 세션 집합의 근거, SessionExited가 종료 마킹 — 이벤트 순서대로
/// 갱신해 한 drain 안의 Exited → pane 제거 MuxUpdated 시퀀스도 정확히 반영된다.
#[derive(Default)]
struct LiveSessionTracker {
    /// 최신 MuxUpdated 기준 pane에 붙은 세션 집합.
    mux_sessions: std::collections::HashSet<runtime::SessionId>,
    /// SessionExited를 관측한 세션 (mux_sessions에 남은 것만 유지해 유계).
    exited_sessions: std::collections::HashSet<runtime::SessionId>,
    /// Spawn 이벤트로 확인한 세션 종류. MuxUpdated가 먼저 오므로 종류 미확인 창은
    /// unknown으로 남겨 suspend를 보수적으로 막는다.
    session_kinds: std::collections::HashMap<runtime::SessionId, runtime::SpawnKind>,
    /// MuxUpdated를 한 번이라도 관측했다 — 관측 전에는 restore 유예가 적용된다.
    seen_mux: bool,
}

impl LiveSessionTracker {
    fn observe(&mut self, event: &runtime::RuntimeEvent) {
        match event {
            runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                self.seen_mux = true;
                self.mux_sessions = snapshot
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .filter_map(|pane| pane.session_id)
                    .collect();
                self.exited_sessions
                    .retain(|s| self.mux_sessions.contains(s));
                self.session_kinds
                    .retain(|s, _| self.mux_sessions.contains(s));
            }
            runtime::RuntimeEvent::ShellSpawned { session } => {
                self.session_kinds
                    .insert(*session, runtime::SpawnKind::Shell);
            }
            runtime::RuntimeEvent::AgentSpawned { session } => {
                self.session_kinds
                    .insert(*session, runtime::SpawnKind::Agent);
            }
            runtime::RuntimeEvent::SessionExited { session, .. }
            // 재시작 시 archived 복원된 세션도 이미 종료됨 — 생존 추적에서 제외해야
            // auto-suspend/warm 축출이 정상 동작한다 (PR-A2 codex 리뷰 P1).
            | runtime::RuntimeEvent::SessionRestored { session, .. } => {
                self.exited_sessions.insert(*session);
            }
            _ => {}
        }
    }

    fn has_live(&self) -> bool {
        self.mux_sessions
            .iter()
            .any(|s| !self.exited_sessions.contains(s))
    }

    /// live 세션이 하나 이상이고 전부 명시적으로 Shell일 때만 목록을 반환한다.
    /// MuxUpdated→Spawned 사이 unknown 또는 Agent가 하나라도 있으면 None(작업 보호).
    fn live_shell_sessions(&self) -> Option<Vec<runtime::SessionId>> {
        let live = self
            .mux_sessions
            .iter()
            .copied()
            .filter(|session| !self.exited_sessions.contains(session))
            .collect::<Vec<_>>();
        if live.is_empty()
            || live
                .iter()
                .any(|session| self.session_kinds.get(session) != Some(&runtime::SpawnKind::Shell))
        {
            return None;
        }
        Some(live)
    }
}

/// 실행 중인 remote TLS 서버 + 그 신원 지문(attach 클라이언트 대조용).
/// 원격 worker는 server가 소유(move)한다 — 활성 workspace worker와 별개의 전용 worker라
/// 수명이 서로 얽히지 않는다. Drop/shutdown이 accept 루프·접속·worker를 모두 정리한다.
struct RemoteTlsState {
    server: runtime::RemoteRuntimeServer,
    fingerprint: String,
}

/// 실행 중인 모바일 웹(PWA) 서버 + 페어링 토큰(접속 URL/QR 표시용) — mobile-pwa v3.3 P1.
/// Drop/shutdown이 accept 루프·접속 스레드를 모두 정리한다 (RemoteTlsState 관례).
struct WebRemoteState {
    server: web_remote::WebRemoteServer,
    /// keyring에서 로드한 페어링 토큰 — 서버가 `/?token=` 게이트로 검증하는 값과 동일.
    token: String,
}

/// 세션 알림(완료/입력대기) 주목 상태 — 레일 폭(6px)·1회 펄스 추적 (2026-07-07).
struct SessionAlert {
    status: runtime::SessionStatus,
    /// 사용자가 확인(포커스)했는가 — false면 레일 6px 유지.
    seen: bool,
    /// 알림 도착 시 이미 포커스 중이던 pane의 1회 펄스 시작 시각.
    pulse_started: Option<std::time::Instant>,
}

/// (env key, 값 또는 credential_id) 쌍 목록 — SetSessionDefaultEnv용.
type EnvPairs = Vec<(String, String)>;

fn font_settings_changed(
    config: &Config,
    last_ui_font: &Option<String>,
    last_mono_font: &str,
    last_mono_weight: &str,
) -> bool {
    &config.ui.ui_font != last_ui_font
        || config.terminal.mono_font.as_str() != last_mono_font
        || config.terminal.mono_weight.as_str() != last_mono_weight
}

pub struct App {
    config: Config,
    config_path: PathBuf,
    /// 직전 프레임의 실효 테마(다크 여부) — 바뀌면 터미널 렌더 캐시를 비운다.
    /// System 테마의 OS 레벨 전환은 config_changed를 안 거치므로 매 프레임 감지한다(#7 codex).
    last_theme_dark: bool,
    /// 직전 프레임의 UI 폰트 설정 — 바뀌면 폰트 재등록(hot reload).
    last_ui_font: Option<String>,
    /// 터미널 모노 굵기 변경 감지용(hot reload 트리거).
    last_mono_font: String,
    last_mono_weight: String,
    /// UI 배율 변경 감지용(zoom_factor 재적용 트리거). 첫 프레임 적용을 위해 sentinel로 시작.
    last_ui_scale: f32,
    /// .env mtime 폴링(2s) — 사이드바 OFF면 워처가 없어 .env 변경/삭제 신호가 안 오므로
    /// (존재여부, mtime) 변화를 직접 감지해 재동기화한다(codex — stale secret 주입 방지).
    last_dotenv_check: std::time::Instant,
    last_dotenv_state: Option<DotenvState>,
    dotenv_sync_worker: DotenvSyncWorker,
    /// 활성 workspace/root가 바뀔 때 증가한다. 옛 worker 결과가 새 workspace runtime에
    /// 주입되는 것을 막는 epoch이다.
    dotenv_sync_generation: u64,
    /// 같은 workspace/root에서도 watcher/manual 변경이 들어오면 증가해 실행 중이던 옛
    /// 파일 snapshot 결과를 폐기한다. 주기적 unchanged poll은 증가시키지 않는다.
    dotenv_sync_revision: u64,
    dotenv_sync_context: Option<(String, Option<PathBuf>)>,
    dotenv_sync_pending: bool,
    dotenv_sync_worker_failed: bool,
    /// worker가 느린 동안 들어온 watcher/poll 요청은 최신 한 건으로 합친다.
    dotenv_sync_deferred: Option<DotenvSyncJob>,
    /// 프로젝트 폴더 rename 복구 확인 모달 — Some((old, new))이면 표시(2026-07-08).
    workspace_rename_prompt: Option<(String, String)>,
    /// 프로젝트 삭제 확인 대기 — Some((id, 표시명)). 확인 모달에서 확정/취소(2026-07-10).
    ws_delete_confirm: Option<(String, String)>,
    /// runtime durable 이벤트 큐가 포화돼 느린 구독자가 끊긴 경우 사용자 경고 모달.
    runtime_stream_warning: bool,
    /// live warm hard cap을 넘기는 workspace 전환을 거부했을 때 대상 표시명.
    warm_limit_warning: Option<String>,
    /// 폰(미러 진입 — I1b-2)이 보낸 워크스페이스 전환 요청 큐. 웹 스레드가 push하고 egui
    /// 스레드가 ui() 시작에서 drain해 switch_workspace로 넘긴다(App은 egui 스레드 소유).
    web_switch_queue: Arc<std::sync::Mutex<Vec<String>>>,
    /// 폰에 띄울 일시 안내(전환 상한 초과 등 — I1b-2)와 세팅 시각. TTL이 지나면 프레임
    /// push에서 None으로 돌려 배너를 내린다(데스크탑 warm_limit_warning 모달과 독립).
    web_notice: Option<(String, std::time::Instant)>,
    /// rename 제안을 '무시'한 워크스페이스 — 이번 실행 동안 재확인 안 함(경로 변경 시 해제).
    dismissed_renames: std::collections::HashSet<String>,
    settings_open: bool,
    /// 직전 프레임의 설정창 열림 상태 — 닫힘 전이에서 env 평문 캐시를 비운다(보안).
    settings_was_open: bool,
    /// 통합 설정 창의 선택된 카테고리.
    settings_category: ui::settings::Category,
    settings_search: String,
    env_api_project_edit: EnvApiProjectEditState,
    /// env/API 프로젝트 행 캐시 (행, 계산 시각) — 설정창이 열려 있는 동안 매 프레임
    /// N+1 SQLite 조회(list_credentials + 워크스페이스별 list_env_profiles/list_env_vars)를
    /// 막는다. 무효화: refresh_workspaces / sync_dotenv_env(명시) + 1s TTL(설정 UI 안에서의
    /// env var·credential 직접 편집은 하위 UI 내부 상태라 TTL로 최대 1s 지연 반영).
    env_api_projects_cache: Option<(Vec<ui::env_project_list::EnvProjectRow>, std::time::Instant)>,
    /// T1: pane 우클릭 → 환경설정 진입 시 감지한 focused 세션 폴더 배너.
    /// 우클릭 진입 시점에만 계산하고, 버튼 클릭 또는 설정 창 닫힘에 버린다.
    env_session_banner: Option<EnvSessionCwdBanner>,
    env_project_rows_worker: EnvProjectRowsWorker,
    env_project_rows_generation: u64,
    env_project_rows_pending: bool,
    env_project_rows_failed: bool,
    env_secret_reveal_worker: EnvSecretRevealWorker,
    env_secret_generation: u64,
    env_secret_cache: std::collections::HashMap<String, String>,
    env_secret_pending: std::collections::HashSet<String>,
    env_secret_failures: std::collections::HashSet<String>,
    db: Db,
    secret_store: KeyringSecretStore,
    agents_ui: ui::agents::AgentsUi,
    /// PTY와 분리된 Codex App Server structured session controller.
    agent_sessions_ui: ui::agent_sessions::AgentSessionsUi,
    /// DB mutation은 controller projection과 분리돼 실패할 수 있다. 성공할 때까지 FIFO로
    /// 보존해 새 thread가 복구 불가능해지거나 archive가 재시작 후 되살아나는 것을 막는다.
    agent_persistence_queue: Vec<ui::agent_sessions::AgentSessionPersistenceMutation>,
    agent_persistence_retry_at: Option<std::time::Instant>,
    connectors_ui: ui::connectors::ConnectorsUi,
    /// OAuth refresh single-flight 조율자 (H5) — 프로세스 단일 인스턴스의 원본.
    /// 현재 소비자는 connectors_ui뿐이지만, 후속 소비자(P2 web 브리지 등)도 반드시
    /// 이 인스턴스의 Arc 클론을 받아야 single-flight가 성립한다 (H4 규약).
    #[allow(dead_code)]
    refresh_coordinator: Arc<auth::RefreshCoordinator>,
    credentials_ui: ui::credentials::CredentialsUi,
    env_profiles_ui: ui::env_profiles::EnvProfilesUi,
    activity_ui: ui::activity::ActivityUi,
    notifications_ui: ui::notifications::NotificationsUi,
    /// 벨 팝오버 「대기 중」 섹션의 PTY 입력 대기 카드 렌더 상태 (v3.9 N3) — 자유 입력칸
    /// 버퍼 + 로그 tail 미리보기 캐시. 팝오버가 열려 있을 때만 조회한다(idle 비용 0).
    inbox_waiting_ui: ui::inbox_waiting::InboxWaitingUi,
    /// agent-proxy 승인 팝업 (option 1.5). proxy가 DB에 쓴 pending 행을 폴링해 표시한다.
    approvals_ui: ui::approvals::ApprovalsUi,
    /// watcher가 pending approval 목록 변화를 감지하면 logic()이 한 번만 DB를 읽게 하는 플래그.
    approval_poll_requested: Arc<AtomicBool>,
    /// 외부 proxy가 DB에 쓴 pending approval 변화를 감지해 UI를 깨운다.
    approval_watcher: ApprovalWatcher,
    /// 마지막 오프스크린 창 위치 보정 시각 (쿨다운용)
    last_offscreen_fix: std::time::Instant,
    /// 시작 시 창을 주 화면으로 1회 이동했다 (centered의 macOS 좌표 문제 우회)
    startup_positioned: bool,
    frame_stats: crate::perf::FrameStats,
    /// 렌더러 A/B 실측 드라이버 (B1) — env 미설정이면 None이고 모든 훅이 no-op이다.
    bench: Option<crate::bench::Bench>,
    i18n: i18n::Catalog,
    /// 현재 활성(렌더되는) workspace의 런타임 상태.
    active: WorkspaceRuntime,
    /// warm workspace들 (전환으로 물러났지만 워커는 계속 실행 — §14.1 Warm). 이벤트는
    /// drain만 하고(채널 backup 방지) 렌더/알림은 안 한다. 재활성 시 즉시 복귀.
    warm: std::collections::HashMap<String, WorkspaceRuntime>,
    /// warm LRU 순서 (앞이 가장 오래됨) — max_warm 초과 시 앞에서부터 Suspended(shutdown).
    warm_order: Vec<String>,
    egui_ctx: egui::Context,
    db_path: PathBuf,
    logs_base: PathBuf,
    redaction: secret::RedactionService,
    workspaces: Vec<crate::storage::WorkspaceRow>,
    /// 런타임이 없는 workspace도 활동 화면에 복원 대상 pane을 표시하기 위한 DB snapshot.
    /// refresh_workspaces에서 한 쿼리로 갱신한다.
    /// 워크스페이스별 영속 pane snapshot — (raw 제목, 세션 cwd). cwd는 기본 제목
    /// ("셸 N")을 프로젝트명으로 표시하는 데 쓴다(활성 워크스페이스와 같은 규칙).
    persisted_activity_panes: std::collections::HashMap<String, Vec<(String, String)>>,
    /// 옵션2: 활성 세션별 에이전트 transcript 활동(working/idle) — 레일 상태에 반영.
    agent_activity:
        std::collections::HashMap<runtime::SessionId, crate::agent_transcript::AgentActivity>,
    /// 세션 → 바인딩된 에이전트(transcript 경로 포함). 바인딩 폴(느림)에서 갱신,
    /// 활동 폴(빠름)이 이걸 재파싱한다.
    agent_bindings:
        std::collections::HashMap<runtime::SessionId, crate::agent_detect::AgentBinding>,
    /// agent 감지 백그라운드 워커(ps/lsof/transcript 스캔을 UI 스레드 밖에서, codex #3).
    /// 필드로 보유만 한다 — App drop 시 이 필드의 Drop이 스레드를 stop+join한다(직접 read X).
    #[allow(dead_code)]
    agent_detect_worker: crate::agent_detect_worker::AgentDetectWorker,
    /// 워커 입력(활성 세션 pid 목록 + epoch) — 매 프레임 최신값 write-through.
    agent_detect_input: crate::agent_detect_worker::DetectInput,
    /// 워커 결과 수신 채널.
    agent_detect_rx: std::sync::mpsc::Receiver<crate::agent_detect_worker::DetectOutcome>,
    /// 워크스페이스 전환마다 증가 — 스레드가 실어 보낸 stale 결과를 폐기하는 데 쓴다.
    agent_detect_epoch: u64,
    /// hook 바인딩 DB 조회 스로틀(1s) — poll_agent_detect는 매 프레임 돌아 매번 쿼리하면
    /// 렌더 중 초당 수십 회가 된다. 캐시를 워커 입력에 재사용.
    last_hook_query: std::time::Instant,
    hook_overrides:
        std::collections::HashMap<runtime::SessionId, crate::agent_detect::AgentBinding>,
    /// 마지막으로 DB에 저장한 pane_id → row — 차등 upsert/delete 및 churn 방지용.
    persisted_agents: std::collections::HashMap<String, crate::storage::AgentSessionRow>,
    /// hook이 보고한 입력 대기(needsInput) 세션들 — DB에서 주기적으로 읽어 레일 주황 반영.
    agent_needs_input: std::collections::HashSet<runtime::SessionId>,
    /// v3.9 N3: 전역(모든 워크스페이스) 입력 대기 — 벨 팝오버 PTY 카드의 소스. 세션 키
    /// (`{workspace_id}:{u64}`) 그대로를 (workspace_id, SessionId)로 파싱만 해 둔다 —
    /// 제목/미리보기 등 표시용 데이터는 팝오버가 열렸을 때만 지연 해석한다(idle 비용 0).
    /// agent_needs_input(활성 전용, 사이드바/상태 레일이 쓴다)과는 별개 필드다.
    global_waiting: Vec<(String, runtime::SessionId)>,
    /// hook이 보고한 턴 완료(Stop) 세션 → updated_at — 레일 '완료(바이올렛)' 트랜지언트
    /// 소스. 값(updated_at)은 소비 시 조건부 clear의 세대 기준(레이스 방지, codex 리뷰).
    agent_turn_done: std::collections::HashMap<runtime::SessionId, i64>,
    /// 완료/입력대기 주목(attention) 추적 — 미확인이면 레일 6px, 포커스 확인 시 해제.
    session_alerts: std::collections::HashMap<runtime::SessionId, SessionAlert>,
    /// 세션별 현재 작업 폴더(감지 워커 lsof) — 행 1행 폴더명 + 워크스페이스명.
    session_cwds: std::collections::HashMap<runtime::SessionId, String>,
    /// 세션별 에이전트 표시 정보(model/effort/context) — 워커 raw(transcript). claude는
    /// effort/context를 statusLine DB(아래)에서 병합해 최종본을 WorkspaceUi로 넘긴다.
    agent_info: std::collections::HashMap<runtime::SessionId, crate::agent_detect::AgentDisplay>,
    /// claude statusLine이 보고한 effort/model/context% — 1s 스로틀로 DB에서 읽어 병합.
    statuslines: std::collections::HashMap<runtime::SessionId, crate::storage::StatuslineRow>,
    /// 복원용으로 로드한 (pane_id → 저장된 에이전트 세션). 워크스페이스 활성 시 로드.
    restore_agents: std::collections::HashMap<String, crate::storage::AgentSessionRow>,
    /// restore_agents를 로드한 워크스페이스 id (전환 시 재로드 판정).
    restore_loaded_for: Option<String>,
    /// 이번 실행에서 이미 resume 명령을 보낸 pane (중복 주입 방지).
    resumed_panes: std::collections::HashSet<String>,
    /// 알림 클릭으로 다른 workspace 전환 후, mux 재구성되면 이동할 (workspace, session).
    pending_focus: Option<(String, runtime::SessionId)>,
    /// 전환으로 background 정리 중인 옛 워커 shutdown 스레드들 (workspace_id, handle).
    /// 앱 종료 시 join(자식 reap 보장) + 같은 workspace 재오픈 전 직렬화(layout 경합 방지).
    pending_shutdowns: Vec<(String, std::thread::JoinHandle<()>)>,
    /// remote TLS 서버 (켜져 있을 때만 Some). 활성 workspace worker와 별개의 전용 worker를 노출.
    remote: Option<RemoteTlsState>,
    /// remote 시작 실패 시 settings에 표시할 에러 (best-effort — 앱은 계속, 크래시 금지).
    remote_error: Option<String>,
    /// settings의 토큰 표시(reveal) 토글. 토큰은 민감이라 기본 마스킹.
    remote_reveal_token: bool,
    /// 모바일 웹(PWA) 서버 (켜져 있을 때만 Some). OFF면 리스너 스레드 자체가 없다 — 리소스 0.
    web: Option<WebRemoteState>,
    /// 웹서버 시작/토큰 재발급 실패 시 settings에 표시할 에러.
    web_error: Option<String>,
    /// settings의 접속 URL 표시(reveal) 토글 — URL에 페어링 토큰이 실리므로 기본 마스킹.
    web_reveal_url: bool,
    /// 접속 URL QR 텍스처 캐시 — URL이 바뀔 때만 재생성, 설정창 닫으면 반환.
    web_qr: ui::settings::WebQrCache,
    /// ts.net 호스트명 자동 감지 1회성 스레드의 결과 수신 (진행 중일 때만 Some).
    ts_detect_rx: Option<std::sync::mpsc::Receiver<crate::tailscale::Detected>>,
    /// 마지막 감지 결과 — 설정 UI 표시용. None = 이 세션에서 아직 시도 안 함.
    ts_detected: Option<crate::tailscale::Detected>,
    /// 이번 감지가 수동 버튼 유래인가 — true면 기존 설정값도 감지값으로 덮어쓴다.
    ts_detect_overwrite: bool,
    /// 웹 스냅샷 마지막 동기화 시각 — 프레임마다 구축하지 않도록 스로틀(리뷰 P2-2).
    last_web_sync: Option<std::time::Instant>,
    /// (스타일, cwd) → 프로젝트 표시명 캐시. Repo 스타일은 .git 상향 stat을 하므로
    /// 활동 패널/웹 스냅샷의 세션별 호출을 메모이즈한다. 키에 스타일을 포함해
    /// 설정 전환 시 별도 무효화가 필요 없다(유계).
    project_name_cache: std::cell::RefCell<
        std::collections::HashMap<(crate::config::SessionNameStyle, String), Option<String>>,
    >,
    /// serve 진단/설정 1회성 스레드의 결과 수신 (진행 중일 때만 Some) — O1.
    serve_rx: Option<std::sync::mpsc::Receiver<crate::tailscale::ServeState>>,
    /// 마지막 serve 진단 결과. None = 이 세션에서 아직 진단 안 함.
    serve_state: Option<crate::tailscale::ServeState>,
    /// known_hosts 표시 캐시 (settings 열 때 lazily 로드, 닫으면 None으로 리셋해 재로드).
    known_hosts_cache: Option<Vec<(String, String)>>,
    /// 폴더 트리 사이드바 (file-tree-design §6). OFF면 None — Panel 미생성 + 상태 drop(리소스 0).
    file_tree: Option<ui::file_tree::FileTreeUi>,
}

fn apply_agent_persistence_batch(
    db: &Db,
    queue: &mut Vec<ui::agent_sessions::AgentSessionPersistenceMutation>,
) -> anyhow::Result<()> {
    use ui::agent_sessions::AgentSessionPersistenceMutation as Mutation;

    let pending = std::mem::take(queue);
    let mut pending = pending.into_iter();
    while let Some(mutation) = pending.next() {
        let result = match &mutation {
            Mutation::Upsert {
                local_session_id,
                workspace_id,
                thread_id,
                title,
                cwd,
                model,
                favorite,
                archived,
            } => db.upsert_structured_thread(
                local_session_id,
                workspace_id,
                thread_id,
                title,
                cwd,
                model.as_deref(),
                *favorite,
                *archived,
            ),
            Mutation::SetArchived {
                local_session_id,
                archived,
            } => db
                .set_structured_thread_archived(local_session_id, *archived)
                .map(|_| ()),
            Mutation::Delete { local_session_id } => {
                db.delete_structured_thread(local_session_id).map(|_| ())
            }
        };
        if let Err(error) = result {
            // 같은 local_session_id의 후속 archive/delete가 앞선 upsert를 추월하면
            // 재시작 복구 상태가 뒤집힌다. 첫 실패부터 남은 FIFO 전체를 보존한다.
            queue.push(mutation);
            queue.extend(pending);
            return Err(error);
        }
    }
    Ok(())
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mut config: Config,
        config_path: PathBuf,
        db: Db,
        workspace_id: String,
        logs_base: PathBuf,
        db_path: PathBuf,
        egui_ctx: egui::Context,
        bench: Option<crate::bench::Bench>,
    ) -> Self {
        // output_batch_ms는 시작 시 고정, scrollback_lines는 세션 spawn 시점에 전달
        config.ui.last_workspace_id = Some(workspace_id.clone());
        // 벤치(B1): DEPPY_BENCH_WORKSPACES=N개가 실제로 상주해야 RSS 비교가 성립한다.
        // warm 상한은 **설정값**이므로(코드 경로 변경 아님) 벤치 임시 config에서만 올린다.
        // clamp(max_warm ≤ 8) 때문에 실효 상한은 active 1 + warm 8 = 9개다.
        if let Some(bench) = &bench {
            let wanted = bench.opts.workspaces.saturating_sub(1).min(8) as u32;
            config.performance.max_warm = config.performance.max_warm.max(wanted);
            config.performance.max_live_warm = config.performance.max_live_warm.max(wanted + 1);
        }
        let redaction = secret::RedactionService::new();
        let i18n = load_catalog(&config.i18n.locale);
        // shim을 make_runtime 전에 설치한다 — 첫 셸부터 PATH에 shim이 얹히도록.
        if config.ui.agent_status_hooks
            && let Ok(bin) = crate::ui::agents::mcp_proxy_bin()
            && let Err(e) = crate::agent_shim::install(&db_path, &bin)
        {
            tracing::warn!("agent shim 설치 실패: {e:#}");
        }
        let active = Self::make_runtime(
            &config,
            &logs_base,
            &workspace_id,
            &db_path,
            &redaction,
            &db,
            &egui_ctx,
        );
        // PR-21 부하 하네스 (env로만 활성): hidden 10개 시나리오 자동 구성 (기본 workspace만)
        if crate::perf::harness_enabled() {
            for i in 0..crate::perf::HARNESS_SESSIONS {
                let (command, args) = crate::perf::harness_command(i);
                let _ = active
                    .runtime
                    .send_command(runtime::RuntimeCommand::SpawnAgent {
                        agent_config_id: None,
                        cols: 120,
                        rows: 40,
                        scrollback_lines: config.terminal.scrollback_lines as usize,
                        command,
                        args,
                        env_plain: Vec::new(),
                        env_secrets: Vec::new(),
                        waiting_regex: None,
                        approval_regex: None,
                        error_regex: None,
                        done_regex: None,
                    });
            }
        }

        // OAuth refresh single-flight 조율자 (H5) — **프로세스 단일 인스턴스**를 App이
        // 보관하고 소비자(connectors, 후속 P2 web 브리지 등)는 Arc 클론을 공유한다.
        // 인스턴스가 갈라지면 동시 도구 호출의 refresh 중복 발사(회전 refresh token
        // 재사용 → AS replay 감지로 grant 폐기)를 막지 못한다 (H4 규약).
        let refresh_coordinator = Arc::new(auth::RefreshCoordinator::new());

        let approval_poll_requested = Arc::new(AtomicBool::new(false));
        let approval_watcher = ApprovalWatcher::spawn(
            db_path.clone(),
            egui_ctx.clone(),
            approval_poll_requested.clone(),
            std::time::Duration::from_millis(Self::APPROVAL_POLL_MS),
        );
        // agent 감지 백그라운드 워커 (ps/lsof/transcript 스캔을 UI 스레드 밖에서, codex #3).
        let (agent_detect_worker, agent_detect_input, agent_detect_rx) =
            crate::agent_detect_worker::AgentDetectWorker::spawn(egui_ctx.clone());
        let env_project_rows_worker =
            EnvProjectRowsWorker::spawn(db_path.clone(), egui_ctx.clone());
        let env_secret_reveal_worker = EnvSecretRevealWorker::spawn(egui_ctx.clone());
        let dotenv_sync_worker =
            DotenvSyncWorker::spawn(db_path.clone(), redaction.clone(), egui_ctx.clone());

        // main에서 CreationContext를 받자마자 이 설정으로 폰트를 이미 설치했다. sentinel로
        // 시작하면 첫 프레임에 15MB AppleGothic을 포함한 FontDefinitions를 다시 만들고
        // 전체 TTF equality 비교까지 하므로, 실제 설치 상태를 초기 snapshot으로 쓴다.
        let last_ui_font = config.ui.ui_font.clone();
        let last_mono_font = config.terminal.mono_font.clone();
        let last_mono_weight = config.terminal.mono_weight.clone();
        let mut app = Self {
            config,
            config_path,
            last_theme_dark: true,
            last_ui_font,
            last_mono_font,
            last_mono_weight,
            last_ui_scale: -1.0,
            last_dotenv_check: std::time::Instant::now(),
            last_dotenv_state: None,
            dotenv_sync_worker,
            dotenv_sync_generation: 0,
            dotenv_sync_revision: 0,
            dotenv_sync_context: None,
            dotenv_sync_pending: false,
            dotenv_sync_worker_failed: false,
            dotenv_sync_deferred: None,
            workspace_rename_prompt: None,
            ws_delete_confirm: None,
            runtime_stream_warning: false,
            warm_limit_warning: None,
            web_switch_queue: Arc::new(std::sync::Mutex::new(Vec::new())),
            web_notice: None,
            dismissed_renames: std::collections::HashSet::new(),
            settings_open: false,
            settings_was_open: false,
            settings_category: ui::settings::Category::default(),
            settings_search: String::new(),
            env_api_project_edit: EnvApiProjectEditState::default(),
            env_api_projects_cache: None,
            env_session_banner: None,
            env_project_rows_worker,
            env_project_rows_generation: 0,
            env_project_rows_pending: false,
            env_project_rows_failed: false,
            env_secret_reveal_worker,
            env_secret_generation: 0,
            env_secret_cache: std::collections::HashMap::new(),
            env_secret_pending: std::collections::HashSet::new(),
            env_secret_failures: std::collections::HashSet::new(),
            db,
            secret_store: KeyringSecretStore,
            agents_ui: ui::agents::AgentsUi::new(),
            agent_sessions_ui: ui::agent_sessions::AgentSessionsUi::new(),
            agent_persistence_queue: Vec::new(),
            agent_persistence_retry_at: None,
            connectors_ui: ui::connectors::ConnectorsUi::new(
                redaction.clone(),
                Arc::new(KeyringSecretStore),
                Arc::clone(&refresh_coordinator),
            ),
            refresh_coordinator,
            credentials_ui: ui::credentials::CredentialsUi::new(),
            env_profiles_ui: ui::env_profiles::EnvProfilesUi::new(),
            activity_ui: ui::activity::ActivityUi::new(),
            notifications_ui: ui::notifications::NotificationsUi::new(),
            inbox_waiting_ui: ui::inbox_waiting::InboxWaitingUi::new(),
            approvals_ui: ui::approvals::ApprovalsUi::new(),
            approval_poll_requested,
            approval_watcher,
            last_offscreen_fix: std::time::Instant::now(),
            startup_positioned: false,
            active,
            warm: std::collections::HashMap::new(),
            warm_order: Vec::new(),
            frame_stats: crate::perf::FrameStats::new(),
            bench,
            i18n,
            egui_ctx,
            db_path,
            logs_base,
            redaction,
            workspaces: Vec::new(),
            persisted_activity_panes: std::collections::HashMap::new(),
            agent_activity: std::collections::HashMap::new(),
            agent_bindings: std::collections::HashMap::new(),
            agent_detect_worker,
            agent_detect_input,
            agent_detect_rx,
            agent_detect_epoch: 0,
            last_hook_query: std::time::Instant::now(),
            hook_overrides: std::collections::HashMap::new(),
            persisted_agents: std::collections::HashMap::new(),
            agent_needs_input: std::collections::HashSet::new(),
            global_waiting: Vec::new(),
            agent_turn_done: std::collections::HashMap::new(),
            session_alerts: std::collections::HashMap::new(),
            session_cwds: std::collections::HashMap::new(),
            agent_info: std::collections::HashMap::new(),
            statuslines: std::collections::HashMap::new(),
            restore_agents: std::collections::HashMap::new(),
            restore_loaded_for: None,
            resumed_panes: std::collections::HashSet::new(),
            pending_focus: None,
            pending_shutdowns: Vec::new(),
            remote: None,
            remote_error: None,
            remote_reveal_token: false,
            web: None,
            web_error: None,
            web_reveal_url: false,
            web_qr: None,
            ts_detect_rx: None,
            ts_detected: None,
            ts_detect_overwrite: false,
            last_web_sync: None,
            project_name_cache: std::cell::RefCell::new(std::collections::HashMap::new()),
            serve_rx: None,
            serve_state: None,
            known_hosts_cache: None,
            file_tree: None,
        };
        app.prune_resolved_approvals();
        // hook 상태 테이블 오래된 행 정리(무한 누적 방지).
        if let Err(e) = app.db.prune_agent_hook_state() {
            tracing::warn!("hook 상태 정리 실패: {e:#}");
        }
        app.poll_pending_approvals();
        // 파일 트리 헤더(workspace 이름) 표시용 — 시작 시 1회 로드
        app.refresh_workspaces();
        if app.config.ui.file_tree_enabled {
            app.file_tree = Some(app.make_file_tree());
        }
        // 에이전트 상태 hook 전역 설치/해제 (설정 토글에 따라, best-effort).
        app.sync_agent_hooks();
        // .env → 환경 profile 동기화 + 새 셸 기본 env 주입 (2026-07-07).
        app.sync_dotenv_env();
        // 시작 시 config가 remote를 켜 뒀으면 best-effort로 기동한다 (실패는 log + settings 표시,
        // config는 그대로 두어 다음 실행에 재시도). 자동 시작은 config 저장을 유발하지 않는다.
        if app.config.remote.tls_enabled {
            match app.start_remote() {
                Ok(state) => app.remote = Some(state),
                Err(e) => {
                    tracing::warn!("remote TLS 자동 시작 실패: {e:#}");
                    app.remote_error = Some(format!("{e:#}"));
                }
            }
        }
        // 모바일 웹(PWA) 서버 자동 시작 — remote와 동일한 best-effort 규칙 (v3.3 P1).
        if app.config.web.enabled {
            match app.start_web() {
                Ok(state) => app.web = Some(state),
                Err(e) => {
                    tracing::warn!("모바일 웹 서버 자동 시작 실패: {e:#}");
                    app.web_error = Some(format!("{e:#}"));
                }
            }
        }
        app
    }

    /// warm 상한(빈 warm 유지 수 `max_warm`, live warm hard cap `max_live_warm`)은 설정
    /// (성능)으로 조정한다 — `self.config.performance`. 기본값은 RAM 유도(config.rs).
    /// 폰 미러 진입(I1b-2) 안내 배너 표시 시간 — 이 뒤 앱이 notice를 None으로 돌린다.
    const WEB_NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(6);
    /// Warm workspace가 이 시간 동안 재활성화되지 않으면 Suspended로 내린다. 세션/PTY는
    /// 종료되고 layout/session metadata만 DB에 남는다 (§14.1). 에이전트·자식 작업은
    /// 계속 보호하고, 단일 저CPU 셸 리더만 남은 경우에만 fresh 셸 복원 전제로 내린다.
    const WARM_AUTO_SUSPEND_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

    /// 승인 watcher 폴링 간격(ms). frame 예약은 하지 않고, pending 상태 변화 때만 UI를 깨운다.
    const APPROVAL_POLL_MS: u64 = 500;
    const RESOLVED_APPROVAL_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

    /// 한 workspace의 런타임 워커를 만든다: 생성 → wake 구독 → 저장 layout 복원 →
    /// credential redaction 시드. (perf 하네스는 제외 — new()에서 기본 workspace만.)
    #[allow(clippy::too_many_arguments)]
    fn make_runtime(
        config: &Config,
        logs_base: &std::path::Path,
        workspace_id: &str,
        db_path: &std::path::Path,
        redaction: &secret::RedactionService,
        db: &Db,
        egui_ctx: &egui::Context,
    ) -> WorkspaceRuntime {
        // 세션 로그 루트: logs/<workspace_id>/ (설계문서 7장)
        let logs_root = logs_base.join(workspace_id);
        // 셸 cwd = workspace 폴더(존재하는 디렉터리일 때만) — 재시작 시 루트가 아닌 이 폴더에서
        // 셸이 떠 claude/codex를 이어갈 수 있다(#2). 미설정/무효면 None(앱 cwd 상속).
        let shell_cwd = db
            .workspace_path(workspace_id)
            .ok()
            .flatten()
            .map(PathBuf::from)
            .filter(|p| p.is_dir());
        let runtime = InProcessRuntimeClient::new(
            config.performance.output_batch_ms,
            Arc::new(KeyringSecretStore),
            logs_root,
            redaction.clone(),
            Some(runtime::PersistConfig {
                db_path: db_path.to_path_buf(),
                workspace_id: workspace_id.to_owned(),
            }),
            shell_cwd.clone(),
            Self::shim_shell_env(config),
        );
        // 상태 이벤트 도착 시 UI를 깨운다 (§14.1 Warm 알림 유지). subscribe→restore 순서
        // 를 코드로 보장하려 subscribe 직후 복원 명령을 보낸다.
        let runtime_events = Self::subscribe_runtime_events(&runtime, egui_ctx);
        // RestoreWorkspace는 background dotenv 결과를 적용한 뒤 보낸다. `.env`/keychain I/O를
        // UI thread에서 수행하지 않으면서도 복원된 첫 셸부터 올바른 기본 env를 받게 한다.
        // 저장된 credential을 로그 redaction 대상으로 시드 (값 resolve는 worker에서)
        Self::seed_redaction(&runtime, db);
        WorkspaceRuntime {
            id: workspace_id.to_owned(),
            runtime,
            events: runtime_events,
            workspace_ui: ui::workspace::WorkspaceUi::new(),
            render_active: true,
            pending_events: Vec::new(),
            session_titles: std::collections::HashMap::new(),
            resource_usage: None,
            session_resource_usage: Vec::new(),
            input_pressure: None,
            session_input_pressure: std::collections::HashMap::new(),
            backgrounded_at: None,
            live: LiveSessionTracker::default(),
            created: std::time::Instant::now(),
            pending_agent_spawns: 0,
            restore_pending_since: Some(std::time::Instant::now()),
            event_overflow_pending: false,
            event_resync_pending: false,
        }
    }

    // --- 렌더러 A/B 실측 드라이버 (B1) ---------------------------------------
    // 전부 `self.bench`(env 게이트) 뒤. 워크스페이스 생성/전환/삭제는 **실제 앱 경로**
    // (DB workspace + runtime worker)를 그대로 탄다 — 그래야 실측이 의미가 있다.

    fn bench_step(&mut self, ctx: &egui::Context) {
        let Some(mut bench) = self.bench.take() else {
            return;
        };
        bench.set_workspaces(1 + self.warm.len());
        // 종료 중이면 새 작업을 시작하지 않는다 (on_exit이 깨끗이 정리되도록).
        if !bench.closing() {
            if bench.needs_setup() {
                self.bench_setup(&mut bench);
            }
            let now = std::time::Instant::now();
            if bench.switch_due(now) {
                self.cycle_workspace(1);
            }
            match bench.createdelete_step(now) {
                Some(crate::bench::CreateDeleteStep::Create(iter)) => {
                    self.bench_create_and_run(&mut bench, &format!("bench-cd-{iter}"));
                }
                Some(crate::bench::CreateDeleteStep::Delete(id)) => {
                    self.bench_delete_workspace(&mut bench, &id);
                }
                None => {}
            }
            // 드라이버가 워크스페이스를 조작하는 시나리오만 프레임을 요구한다.
            if bench.needs_frames() {
                ctx.request_repaint();
            }
        }
        self.bench = Some(bench);
    }

    fn bench_setup(&mut self, bench: &mut crate::bench::Bench) {
        let base = self.active.id.clone();
        bench.base = Some(base.clone());
        let extra = bench.opts.workspaces.saturating_sub(1);
        if extra > 0 {
            bench.emit_rss_stage("ws_create_begin");
            for i in 0..extra {
                self.bench_create_and_run(bench, &format!("bench-ws-{i}"));
            }
            // 활성은 항상 1개 — 기준 워크스페이스로 복귀(나머지는 warm으로 상주).
            let started = std::time::Instant::now();
            self.switch_workspace(&base);
            bench.emit_ws_step("switch_back", elapsed_ms(started));
            // 상주 수를 먼저 갱신한 뒤 스테이지를 찍는다 — 안 그러면 ws_create_done이
            // 직전 프레임의 값(1)을 달고 나간다.
            bench.set_workspaces(1 + self.warm.len());
            bench.emit_rss_stage("ws_create_done");
        }
        // createdelete는 반복마다 자기 셸을 띄운다 — 기준 워크스페이스는 비워 둔다.
        if bench.scenario() != crate::bench::Scenario::CreateDelete {
            bench.begin_burst();
            self.bench_spawn_scenario(bench);
        }
    }

    /// 워크스페이스를 만들고(DB) 전환한 뒤(runtime worker) 시나리오 셸을 띄운다.
    fn bench_create_and_run(&mut self, bench: &mut crate::bench::Bench, name: &str) {
        let started = std::time::Instant::now();
        let id = match self.db.create_workspace(name) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("벤치 워크스페이스 생성 실패: {e:#}");
                return;
            }
        };
        bench.emit_ws_step("db_create", elapsed_ms(started));
        self.refresh_workspaces();

        let started = std::time::Instant::now();
        self.switch_workspace(&id);
        bench.emit_ws_step("runtime_alloc", elapsed_ms(started));
        if self.active.id != id {
            // warm hard cap이 전환을 거부했다 — 지어내지 말고 사실대로 남긴다.
            tracing::warn!(workspace = %id, "벤치 전환 거부(live warm 상한) — 이 워크스페이스는 미상주");
            bench.emit_ws_step("switch_rejected", 0.0);
            return;
        }
        bench.begin_burst();
        self.bench_spawn_scenario(bench);
        bench.set_createdelete_current(id);
    }

    fn bench_spawn_scenario(&mut self, bench: &mut crate::bench::Bench) {
        let Some((command, args)) = bench.scenario().command() else {
            return;
        };
        let started = std::time::Instant::now();
        let _ = self
            .active
            .runtime
            .send_command(runtime::RuntimeCommand::SpawnAgent {
                agent_config_id: None,
                cols: 120,
                rows: 40,
                scrollback_lines: self.config.terminal.scrollback_lines as usize,
                command,
                args,
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
                waiting_regex: None,
                approval_regex: None,
                error_regex: None,
                done_regex: None,
            });
        bench.emit_ws_step("spawn_send", elapsed_ms(started));
    }

    /// UI의 삭제 경로와 같은 순서: 기준 워크스페이스로 물러난 뒤 warm shutdown + DB 삭제.
    fn bench_delete_workspace(&mut self, bench: &mut crate::bench::Bench, delete_id: &str) {
        let started = std::time::Instant::now();
        if self.active.id == delete_id
            && let Some(base) = bench.base.clone()
        {
            self.switch_workspace(&base);
        }
        if self.active.id == delete_id {
            tracing::warn!(workspace = %delete_id, "벤치: 활성 워크스페이스라 삭제 불가");
            return;
        }
        self.join_pending_shutdown(delete_id);
        if let Some(mut runtime) = self.warm.remove(delete_id) {
            runtime.runtime.shutdown();
        }
        self.warm_order.retain(|id| id != delete_id);
        self.broadcast_terminal_cache_policy();
        self.notifications_ui.prune_workspace(delete_id);
        if let Err(e) = self.db.delete_workspace(delete_id) {
            tracing::warn!("벤치 워크스페이스 삭제 실패: {e:#}");
        }
        self.refresh_workspaces();
        bench.emit_ws_step("delete", elapsed_ms(started));
    }

    fn subscribe_runtime_events(
        runtime: &InProcessRuntimeClient,
        ctx: &egui::Context,
    ) -> RuntimeEventReceiver {
        runtime.subscribe_with_wake(std::sync::Arc::new({
            let ctx = ctx.clone();
            // request_repaint()가 아니라 request_repaint_after(1ms) — egui는 delay==0인
            // 요청마다 "settle" 프레임을 한 장 더 붙인다(egui 0.35 context.rs:137, outstanding=1).
            // 0이 아닌 delay는 그 경로를 타지 않고, 이어서 delay -= predicted_dt로 0이 되어
            // 결국 즉시 리페인트된다 — 지연 없이 헛 프레임만 뺀다. 터미널 내용은 같은 프레임의
            // handle_events()에서 스냅샷이 반영된 뒤 그려지므로 settle 프레임이 필요 없다.
            move || ctx.request_repaint_after(std::time::Duration::from_millis(1))
        }))
    }

    /// 저장된 credential id를 worker의 로그 redaction 대상으로 시드한다 (값 resolve는 worker).
    /// 활성 workspace worker와 remote 전용 worker가 공유하는 시드 로직.
    fn seed_redaction(runtime: &InProcessRuntimeClient, db: &Db) {
        match db.list_credentials() {
            Ok(credentials) => {
                let mut ids: Vec<String> = Vec::with_capacity(credentials.len());
                for c in credentials {
                    if c.credential_kind == "oauth_token" {
                        ids.push(auth::refresh_entry_id(&c.id));
                    }
                    ids.push(c.id);
                }
                if !ids.is_empty()
                    && let Err(e) = runtime.send_command(runtime::RuntimeCommand::SeedRedaction {
                        credential_ids: ids,
                    })
                {
                    tracing::warn!("redaction 시드 전송 실패: {e:#}");
                }
            }
            Err(e) => tracing::warn!("credential 목록 조회 실패 (redaction 시드 생략): {e:#}"),
        }
    }

    /// 에이전트 감지 워커의 입력 갱신 + 결과 드레인 — ui()가 아닌 logic()에서 돈다.
    /// hidden/minimized로 ui()가 스킵돼도 결과를 소비해 unbounded 채널 누적을 막는다
    /// (§14.1 Warm: 창이 안 보이면 logic()만 호출됨, codex 리뷰). UI(egui)에 의존하지 않는
    /// 순수 상태 갱신이라 logic()이 올바른 위치다.
    fn poll_agent_detect(&mut self) {
        // 워커 입력(활성 세션 pid 목록 + epoch)을 최신값으로 갱신 — 워커가 다음 tick에 읽어
        // ps/lsof/transcript 스캔을 UI 스레드 밖에서 수행한다.
        let sessions: Vec<(runtime::SessionId, u32)> = self
            .active
            .session_resource_usage
            .iter()
            .filter_map(|r| r.pid.map(|pid| (r.session, pid)))
            .collect();
        // 터미널 경로 더블클릭의 상대경로 해석용 — 같은 목록을 workspace UI에도 나른다.
        self.active.workspace_ui.set_session_pids(&sessions);
        // hook이 보고한 결정적 바인딩(활성 워크스페이스 것만) — 워커 탐색을 대체한다.
        // poll_agent_detect는 매 프레임 돌므로 DB 조회는 1초 스로틀 + 캐시.
        if self.last_hook_query.elapsed() >= std::time::Duration::from_secs(1) {
            self.last_hook_query = std::time::Instant::now();
            let ws_prefix = format!("{}:", self.active.id);
            self.hook_overrides = self
                .db
                .list_hook_sessions()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|r| {
                    let sid = r
                        .session_key
                        .strip_prefix(&ws_prefix)?
                        .parse::<u64>()
                        .ok()?;
                    let kind = crate::agent_detect::kind_from_str(&r.kind)?;
                    Some((
                        runtime::SessionId(sid),
                        crate::agent_detect::AgentBinding {
                            kind,
                            session_id: r.agent_session_id,
                            transcript: std::path::PathBuf::from(r.transcript_path),
                        },
                    ))
                })
                .collect();
            // claude statusLine 표시 정보(effort/model/context%) — 활성 워크스페이스 것만.
            self.statuslines = self
                .db
                .list_statuslines()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|r| {
                    let sid = r
                        .session_key
                        .strip_prefix(&ws_prefix)?
                        .parse::<u64>()
                        .ok()?;
                    Some((runtime::SessionId(sid), r))
                })
                .collect();
        }
        if let Ok(mut input) = self.agent_detect_input.lock() {
            *input = (
                self.agent_detect_epoch,
                sessions,
                self.hook_overrides.clone(),
                // 창 숨김(가림/최소화) — detect 스레드가 ps/lsof/transcript 폴링을 완화한다.
                !self.active.render_active,
            );
        }
        // 결과를 논블로킹 드레인 — 최신 것만 취한다(epoch 불일치=전환 잔여는 폐기).
        let mut latest_bindings = None;
        let mut latest_activity = None;
        let mut latest_cwds = None;
        let mut latest_info = None;
        while let Ok(outcome) = self.agent_detect_rx.try_recv() {
            if outcome.epoch != self.agent_detect_epoch {
                continue;
            }
            latest_activity = Some(outcome.activity);
            if outcome.bindings.is_some() {
                latest_bindings = outcome.bindings;
            }
            if outcome.session_cwds.is_some() {
                latest_cwds = outcome.session_cwds;
            }
            if outcome.agent_info.is_some() {
                latest_info = outcome.agent_info;
            }
        }
        if let Some(info) = latest_info {
            self.agent_info = info;
        }
        // 에이전트 표시정보 최종본(claude는 statusLine으로 effort/model/context 병합) →
        // WorkspaceUi. statuslines가 매 1s 갱신되므로 매 poll에서 병합해 최신을 반영한다.
        self.push_agent_display();
        if let Some(cwds) = latest_cwds {
            // 변경된 세션 cwd만 워커 persist로 — 재시작 복원이 pane별 원래 폴더에서
            // 셸을 띄우게 한다(A안 2026-07-08). 같은 값은 워커 쪽에서도 no-op이지만
            // 여기서 걸러 wire 트래픽을 줄인다.
            for (session, cwd) in &cwds {
                if self.session_cwds.get(session) != Some(cwd) {
                    let _ = self.active.runtime.send_command(
                        runtime::RuntimeCommand::UpdateSessionCwd {
                            session: *session,
                            cwd: cwd.clone(),
                        },
                    );
                }
            }
            self.session_cwds = cwds;
            // 세션 행/pane 헤더 1행 폴더명 원천 — WorkspaceUi에 전달.
            self.active
                .workspace_ui
                .set_session_cwds(self.session_cwds.clone(), self.config.ui.session_name_style);
            // 포커스 세션 cwd → 워크스페이스 이름(현재 작업 폴더/프로젝트명).
            if let Some(cwd) = self
                .active
                .workspace_ui
                .focused_session()
                .and_then(|sid| self.session_cwds.get(&sid))
                .cloned()
            {
                self.update_workspace_folder_name(&cwd);
            }
        }
        if let Some(activity) = latest_activity {
            self.agent_activity = activity;
            self.refresh_needs_input();
        }
        if let Some(bindings) = latest_bindings {
            self.agent_bindings = bindings.clone();
            self.process_agent_bindings(&bindings);
        }
    }

    /// hook이 보고한 입력 대기 세션(needsInput)을 DB에서 읽어 갱신한다. session_key는
    /// `{workspace_id}:{session_id}` — SessionId가 워커마다 1부터라 전역 유일하지 않아
    /// workspace_id로 스코프한다(codex High). agent_needs_input은 활성 워크스페이스
    /// 것만 남긴다(사이드바/상태 레일용, 의미 불변).
    fn refresh_needs_input(&mut self) {
        let ws = self.active.id.clone();
        let waiting_keys = self.db.list_waiting_sessions().unwrap_or_default();
        let to_id = |k: &str| -> Option<runtime::SessionId> {
            let (w, s) = k.rsplit_once(':')?;
            (w == ws).then_some(())?;
            s.parse::<u64>().ok().map(runtime::SessionId)
        };
        self.agent_needs_input = waiting_keys.iter().filter_map(|k| to_id(k)).collect();
        // v3.9 N3: 전역(모든 워크스페이스) 대기 — 같은 DB 조회 결과를 재사용해 새 쿼리
        // 없이 벨 팝오버 PTY 카드 소스를 채운다. 표시용 제목/미리보기는 팝오버가 열렸을
        // 때 build_waiting_cards가 지연 해석한다.
        self.global_waiting = waiting_keys
            .iter()
            .filter_map(|k| ui::inbox_waiting::parse_session_key(k))
            .collect();
        // 턴 완료(Stop hook) — 확인(포커스) 시 update_session_alerts가 소비한다.
        // updated_at을 함께 들고 있다가 조건부 clear의 세대 기준으로 쓴다(레이스 방지).
        self.agent_turn_done = self
            .db
            .list_turn_done_sessions()
            .unwrap_or_default()
            .iter()
            .filter_map(|(k, at)| Some((to_id(k)?, *at)))
            .collect();
    }

    /// 워커 raw(agent_info) + claude statusLine(statuslines)을 병합해 최종 표시정보를
    /// WorkspaceUi에 넘긴다. claude는 statusLine의 effort/model/context%를 우선(정확),
    /// 없으면 transcript 값. codex는 raw 그대로.
    fn push_agent_display(&mut self) {
        use crate::agent_detect::{AgentDisplay, AgentKind};
        let mut merged: std::collections::HashMap<runtime::SessionId, AgentDisplay> =
            self.agent_info.clone();
        for (sid, d) in merged.iter_mut() {
            if d.kind == AgentKind::Claude
                && let Some(sl) = self.statuslines.get(sid)
            {
                if sl.effort.is_some() {
                    d.effort = sl.effort.clone();
                }
                if sl.model.is_some() {
                    d.model = sl.model.clone(); // "Opus 4.8 (1M context)" — transcript보다 나음
                }
                if let Some(pct) = sl.context_pct {
                    d.context_pct = Some(pct.clamp(0, 100) as u8);
                }
            }
        }
        self.active.workspace_ui.set_agent_info(merged);
    }

    fn pty_agent_surfaces(
        &self,
        entries: &[ui::file_tree::SessionEntry],
    ) -> Vec<crate::agent_surface::AgentSurfaceSnapshot> {
        entries
            .iter()
            .filter_map(|entry| {
                let session_id = entry.session?;
                let info = self.agent_info.get(&session_id)?;
                Some(crate::agent_surface::AgentSurfaceSnapshot {
                    id: crate::agent_surface::AgentSurfaceId::Pty {
                        workspace_id: self.active.id.clone(),
                        pane_id: entry.pane.0.clone(),
                        session_id,
                    },
                    provider: crate::agent_surface::AgentProvider::from(info.kind),
                    transport: crate::agent_surface::AgentTransport::Pty,
                    title: entry.title.clone(),
                    model: info.model.clone(),
                    effort: info.effort.clone(),
                    context_pct: info.context_pct,
                    state: crate::agent_surface::AgentVisualState::from_pty(entry.status),
                })
            })
            .collect()
    }

    /// 완료/입력대기 주목(attention) 추적 — 세션 엔트리에 attention/pulse를 채운다.
    /// 규칙(2026-07-07): 알림 발생 시 그 pane이 비포커스면 확인할 때까지 레일 6px 유지,
    /// 이미 포커스 중이면 6px 대신 1회 펄스. 완료는 확인 시 소비(DB clear → 유휴로 복귀).
    fn update_session_alerts(&mut self, entries: &mut [ui::file_tree::SessionEntry]) {
        use runtime::SessionStatus as S;
        const PULSE_SECS: f32 = 0.9;
        let mut any_pulse = false;
        for entry in entries.iter_mut() {
            let Some(sid) = entry.session else { continue };
            let alert_status = match entry.status {
                Some(s @ (S::Done | S::NeedsApproval)) => Some(s),
                _ => None,
            };
            match alert_status {
                Some(status) => {
                    let is_new = self
                        .session_alerts
                        .get(&sid)
                        .is_none_or(|a| a.status != status);
                    if is_new {
                        // 새 알림: 보고 있으면 펄스 1회, 아니면 미확인(6px)으로 시작.
                        self.session_alerts.insert(
                            sid,
                            SessionAlert {
                                status,
                                seen: entry.focused,
                                pulse_started: entry.focused.then(std::time::Instant::now),
                            },
                        );
                    }
                    let alert = self.session_alerts.get_mut(&sid).expect("방금 삽입/존재");
                    // 확인: 포커스가 오면 seen 처리. 완료는 소비해 유휴로 되돌린다.
                    if entry.focused && !alert.seen {
                        alert.seen = true;
                    }
                    if alert.seen
                        && status == S::Done
                        && let Some(&seen_at) = self.agent_turn_done.get(&sid)
                    {
                        // 내가 읽은 세대(seen_at)까지만 소비 — 그 뒤 도착한 새 완료는 남긴다.
                        let key = format!("{}:{}", self.active.id, sid.0);
                        let _ = self.db.clear_agent_turn_done(&key, seen_at);
                        self.agent_turn_done.remove(&sid);
                    }
                    entry.attention = !alert.seen;
                    if let Some(started) = alert.pulse_started {
                        let t = started.elapsed().as_secs_f32() / PULSE_SECS;
                        if t < 1.0 {
                            entry.pulse = Some((
                                t,
                                ui::file_tree::session_status_color(
                                    Some(status),
                                    &egui::Visuals::dark(),
                                ),
                            ));
                            any_pulse = true;
                        } else {
                            alert.pulse_started = None;
                        }
                    }
                }
                None => {
                    self.session_alerts.remove(&sid);
                }
            }
        }
        if any_pulse {
            // 펄스 애니메이션 프레임 지속 — 끝나면 자연히 유휴 리페인트로 복귀.
            self.egui_ctx.request_repaint();
        }
    }

    /// 바인딩 감지 결과를 소비한다 — 저장(차등 upsert/delete) + 복원 resume 주입. 워커
    /// 스레드에서 계산된 bindings를 받아 UI 스레드(여기)에서 부수효과만 처리한다(codex #3).
    fn process_agent_bindings(
        &mut self,
        bindings: &std::collections::HashMap<runtime::SessionId, crate::agent_detect::AgentBinding>,
    ) {
        // 복원용 저장 데이터를 먼저 로드한다(아래 persistence가 지우기 전에). 전환 시 재로드.
        if self.restore_loaded_for.as_deref() != Some(self.active.id.as_str()) {
            self.restore_agents = self
                .db
                .list_agent_sessions(&self.active.id)
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.pane_id.clone(), r))
                .collect();
            // persisted_agents는 "이번 세션에 감지해 저장한 것"만 추적한다(빈 맵 시작 — 아직
            // 감지 전인 복원 데이터를 삭제 루프가 지우지 않게, codex).
            self.persisted_agents = std::collections::HashMap::new();
            self.resumed_panes.clear();
            self.restore_loaded_for = Some(self.active.id.clone());
        }

        let mux = self.active.workspace_ui.mux().cloned();
        let current: std::collections::HashMap<String, crate::storage::AgentSessionRow> = bindings
            .iter()
            .filter_map(|(sid, b)| {
                let pane = mux.as_ref().and_then(|m| pane_of_session(m, *sid))?;
                let kind = match b.kind {
                    crate::agent_detect::AgentKind::Claude => "claude",
                    crate::agent_detect::AgentKind::Codex => "codex",
                };
                Some((
                    pane.0.clone(),
                    crate::storage::AgentSessionRow {
                        pane_id: pane.0,
                        kind: kind.to_owned(),
                        session_id: b.session_id.clone(),
                    },
                ))
            })
            .collect();
        for (pane_id, row) in &current {
            if self.persisted_agents.get(pane_id) != Some(row)
                && let Err(e) = self.db.upsert_agent_session(
                    &self.active.id,
                    pane_id,
                    &row.kind,
                    &row.session_id,
                )
            {
                tracing::warn!("agent session 저장 실패: {e:#}");
            }
        }
        for pane_id in self.persisted_agents.keys() {
            if !current.contains_key(pane_id) {
                let _ = self.db.delete_agent_session(&self.active.id, pane_id);
            }
        }
        self.persisted_agents = current;

        // 복원 resume 주입: 저장된 에이전트가 있는 pane에 에이전트가 아직 안 떠 있으면
        // native resume 명령을 셸에 한 번 보낸다(설정으로 끌 수 있다, 기본 ON).
        if self.config.ui.auto_resume_agents
            && let Some(mux) = &mux
        {
            // codex transcript 존재확인은 세션 디렉터리 스캔이라, 복원 pane마다
            // 반복하지 않게 finder가 1회 스캔을 이 pass 전체에 재사용한다.
            let mut transcript_finder = crate::agent_detect::TranscriptFinder::new();
            for pane in mux.tabs.iter().flat_map(|t| &t.panes) {
                let pane_key = pane.id.0.clone();
                if !self.restore_agents.contains_key(&pane_key) {
                    continue;
                }
                let Some(session) = pane.session_id else {
                    continue;
                };
                if self.resumed_panes.contains(&pane_key) || bindings.contains_key(&session) {
                    continue; // 이미 보냈거나 이미 실행 중
                }
                self.send_agent_resume(&pane_key, &pane.title, session, &mut transcript_finder);
                self.resumed_panes.insert(pane_key);
            }
        }
    }

    /// 저장된 에이전트를 pane 셸에 resume 명령으로 주입한다 — 시작 자동 이어가기와
    /// 사이드바 수동 '이어가기'의 공용 경로. transcript가 사라졌으면 저장 행을 지우고
    /// 웹 공지 후 false.
    fn send_agent_resume(
        &mut self,
        pane_key: &str,
        pane_title: &str,
        session: runtime::SessionId,
        finder: &mut crate::agent_detect::TranscriptFinder,
    ) -> bool {
        let Some((saved_kind, saved_sid)) = self
            .restore_agents
            .get(pane_key)
            .map(|saved| (saved.kind.clone(), saved.session_id.clone()))
        else {
            return false;
        };
        // 대상 transcript가 아직 존재하는지 확인 — 지워진 세션에 --resume 안 던짐.
        let kind = crate::agent_detect::kind_from_str(&saved_kind);
        let transcript = kind.and_then(|k| finder.find(k, &saved_sid));
        let Some(transcript) = transcript else {
            let _ = self.db.delete_agent_session(&self.active.id, pane_key);
            // 폰 안내(I1b-3): 기록이 사라져 이어받지 못함 — 셸은 이미 복원돼 있어
            // 조용히 넘어가면 폰 사용자는 에이전트가 왜 없는지 모른다. 제목은
            // 활동 패널과 같은 프로젝트명 규칙으로 해석해 보낸다.
            let title = self.activity_session_name(&self.active.id, pane_title);
            let msg = self
                .i18n
                .t("workspace.wake.resume_missing", &[("title", &title)]);
            self.set_web_notice(Some(msg));
            self.egui_ctx.request_repaint_after(Self::WEB_NOTICE_TTL);
            return false;
        };
        // session_id는 그대로 셸 문자열에 들어간다 — 안전 문자만 허용(비정상
        // transcript/DB 값의 셸 메타문자 실행 방지, codex Low).
        if !saved_sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            tracing::warn!(pane = %pane_key, "비정상 세션 id — resume 생략");
            return false;
        }
        // 세션의 원래 폴더로 cd 후 resume — 셸이 workspace 루트에서 떠서 대화는
        // 이어지는데 실제 작업 폴더가 달랐던 문제(2026-07-08 사용자 #6).
        let cd_prefix = crate::agent_detect::transcript_cwd(&transcript)
            .filter(|p| std::path::Path::new(p).is_dir())
            .map(|p| format!("cd {} && ", crate::agent_hooks::sh_quote(&p)))
            .unwrap_or_default();
        let cmd = match saved_kind.as_str() {
            "claude" => format!("{cd_prefix}claude --resume {saved_sid}\n"),
            "codex" => format!("{cd_prefix}codex resume {saved_sid}\n"),
            _ => return false,
        };
        // 선택 중 freeze 해제 — 이 경로도 WorkspaceUi::send를 우회한다(codex).
        self.active.workspace_ui.clear_selection(session);
        let _ = self
            .active
            .runtime
            .send_command(runtime::RuntimeCommand::WriteInput {
                session,
                bytes: cmd.into_bytes(),
            });
        true
    }

    /// 세션의 현재 작업 폴더 — 감지 워커 캐시 우선, 없으면 pid로 일회성 lsof 조회
    /// (사용자 클릭 시점의 1회 조회라 스폰 비용 감수 — platform::process_cwd 관례).
    fn session_cwd_lookup(&self, session: runtime::SessionId) -> Option<String> {
        if let Some(cwd) = self.session_cwds.get(&session) {
            return Some(cwd.clone());
        }
        let pid = self.active.workspace_ui.session_pid(session)?;
        platform::process_cwd(pid).map(|path| path.to_string_lossy().into_owned())
    }

    /// shim PATH env — hook 토글 ON이고 shim이 설치돼 있으면 셸 PATH 앞에 주입한다.
    fn shim_shell_env(config: &Config) -> Vec<(String, String)> {
        // .env 라이브 반영(E5 ⑨): zsh ZDOTDIR 훅 — 래퍼는 항상 주입(passthrough,
        // 기능 OFF면 no-op)하고 활성 조건은 세션 기본 env가 동적으로 나른다.
        let mut env = crate::env_reload::shell_env();
        if !config.ui.agent_status_hooks {
            return env;
        }
        let Some(dir) = crate::agent_shim::shim_dir() else {
            return env;
        };
        let path = std::env::var("PATH").unwrap_or_default();
        env.push(("PATH".to_owned(), format!("{}:{path}", dir.display())));
        env
    }

    /// 에이전트 상태 hook을 설정 토글에 맞춰 전역 설치/해제한다(옵션2 needsInput).
    /// best-effort — 실패해도 앱은 정상 동작(regex fallback). claude + codex.
    fn sync_agent_hooks(&self) {
        let result = (|| -> anyhow::Result<()> {
            // 전역 config 방식(구)은 항상 정리한다 — shim 방식으로 전환(cmux식, 2026-07-07).
            crate::agent_hooks::uninstall_claude()?;
            crate::agent_hooks::uninstall_codex()?;
            if self.config.ui.agent_status_hooks {
                let bin = crate::ui::agents::mcp_proxy_bin()?;
                crate::agent_shim::install(&self.db_path, &bin)?;
            } else {
                crate::agent_shim::remove()?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!("에이전트 상태 hook 동기화 실패: {e:#}");
        }
    }

    /// 앱 데이터 디렉터리 (db_path = `<data>/metadata.sqlite3` → parent). remote cert/known_hosts의 기준.
    fn data_dir(&self) -> &std::path::Path {
        self.db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
    }

    /// remote TLS 서버 신원 인증서 경로 (`<data>/remote-tls.crt` — tls_identity 관례, 키는 keyring).
    fn cert_path(&self) -> PathBuf {
        self.data_dir().join("remote-tls.crt")
    }

    /// 클라이언트 측 known_hosts 파일 경로 (`<data>/known_hosts`).
    fn known_hosts_path(&self) -> PathBuf {
        self.data_dir().join("known_hosts")
    }

    /// remote TLS 서버를 기동한다: 신원 로드/생성 → 전용 원격 worker(비영속) → loopback bind.
    /// **원격 worker는 fresh empty 런타임**(원격 클라가 스스로 세션을 만든다) + PersistConfig=None
    /// (원격 세션은 영속하지 않는다). 실패는 Err — 호출측이 표시하고 앱은 계속(크래시 금지).
    fn start_remote(&self) -> anyhow::Result<RemoteTlsState> {
        let identity =
            runtime::tls_identity::get_or_create_identity(&self.secret_store, &self.cert_path())?;
        let fingerprint = identity.fingerprint();
        // 전용 원격 worker — logs는 logs_base/remote/ 하위(활성 workspace 로그와 분리).
        let worker = InProcessRuntimeClient::new(
            self.config.performance.output_batch_ms,
            Arc::new(KeyringSecretStore),
            self.logs_base.join("remote"),
            self.redaction.clone(),
            None,       // 원격 세션은 영속하지 않는다
            None,       // 원격은 workspace 폴더 개념 없음 — cwd 상속
            Vec::new(), // 원격 셸엔 shim 미주입
        );
        Self::seed_redaction(&worker, &self.db);
        let addr =
            std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, self.config.remote.port));
        // loopback 전용(allow_non_loopback=false) — 비-loopback 개방은 후속 UI(C-4 가드 유지).
        let server = runtime::RemoteRuntimeServer::serve_tls(worker, addr, identity, false)?;
        Ok(RemoteTlsState {
            server,
            fingerprint,
        })
    }

    /// settings 체크 on: 서버를 켜고 성공 시 config에 의도를 영속한다(다음 실행 자동 시작).
    fn remote_enable(&mut self) {
        match self.start_remote() {
            Ok(state) => {
                self.remote = Some(state);
                self.remote_error = None;
                self.config.remote.tls_enabled = true;
                if let Err(e) = self.config.save(&self.config_path) {
                    tracing::warn!("config 저장 실패: {e:#}");
                    // 서버는 켜졌지만 자동시작이 영속되지 않음 — 사용자에게 알린다.
                    self.remote_error = Some(format!(
                        "설정 저장 실패 — 다음 실행엔 자동시작 안 됨: {e:#}"
                    ));
                }
            }
            Err(e) => {
                tracing::warn!("remote TLS 시작 실패: {e:#}");
                self.remote_error = Some(format!("{e:#}"));
            }
        }
    }

    /// settings 체크 off: 서버를 정지(Drop이 accept/접속/worker 정리)하고 config에 영속한다.
    fn remote_disable(&mut self) {
        if let Some(state) = self.remote.take() {
            state.server.shutdown();
        }
        self.remote_error = None;
        self.config.remote.tls_enabled = false;
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::warn!("config 저장 실패: {e:#}");
            // 저장 실패를 조용히 넘기면 config.toml에 tls_enabled=true가 남아, 사용자가 껐다고
            // 생각한 원격 서버(셸 접근 동등)가 다음 실행에 다시 자동시작된다 — 표면화 (codex P2).
            self.remote_error = Some(format!(
                "서버는 껐지만 설정 저장 실패 — 다음 실행에 다시 켜질 수 있습니다: {e:#}"
            ));
        }
    }

    /// 모바일 웹(PWA) 서버 기동 (mobile-pwa v3.3 P1): keyring 페어링 토큰 로드/생성 →
    /// 127.0.0.1 평문 bind(serve 모드 — HTTPS 종단은 tailscale serve 몫).
    /// cert 모드(자체 TLS + 비-loopback)는 후속 — config에 자리만 있다.
    fn start_web(&self) -> anyhow::Result<WebRemoteState> {
        let token = web_remote::pairing::get_or_create_token(&self.secret_store)?;
        // 웹푸시(P4) VAPID 키 — keyring에서 get_or_create(SecretStore 접근이 app 소유). 개인키는
        // keyring에만, 공개키만 서버가 JS에 노출한다. 실패하면 푸시만 비활성(대시보드는 유지).
        let vapid = match web_remote::push::get_or_create_vapid_key(&self.secret_store) {
            Ok(key) => Some(key),
            Err(e) => {
                tracing::warn!("웹푸시 VAPID 키 준비 실패 — 푸시 비활성: {e:#}");
                None
            }
        };
        let addr =
            std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, self.config.web.port));
        let hostname = self.config.web.ts_hostname.trim();
        let server = web_remote::WebRemoteServer::serve(
            addr,
            web_remote::ServeOptions {
                token: token.clone(),
                allowed_host: (!hostname.is_empty()).then(|| hostname.to_owned()),
                // 승인 대시보드는 자체 Db 연결로 직행(프록시↔GUI 공유 DB IPC 관례, 계획 §0.2).
                db_path: Some(self.db_path.clone()),
                // 웹푸시 발송(P4)도 db_path로 자체 연결을 연다(구독 저장·승인 폴링).
                vapid,
                // 모바일 파일 첨부(P6d) — 세션에 묶이지 않는 평면 디렉터리라 workspace별
                // logs_root가 아닌 logs_base 바로 아래에 둔다(remote/ 분리와 같은 이유).
                uploads_dir: Some(self.logs_base.join("uploads")),
            },
        )?;
        // 활성 workspace worker 이벤트를 대시보드에 붙인다(P2). wake 클로저는 egui 프레임과
        // 무관하게 브리지 스레드를 깨운다(§14.1 Warm 알림 유지) — 창이 숨겨져도 상태가 흐른다.
        // 워크스페이스 전환 시엔 rebind_web_dashboard가 새 worker로 재구독한다.
        let receiver = self
            .active
            .runtime
            // background 구독 — 웹 브리지는 렌더와 무관하므로 원격 전용 Viewport에도
            // 깨어나되(시청 프레임 라우팅), GUI repaint는 유발하지 않는다 (P5 리뷰 P1).
            .subscribe_with_wake_background(server.dashboard_wake());
        // 터미널 뷰어(P5)의 시청 lease를 runtime으로 보낼 명령 싱크 — receiver보다 먼저
        // (set_runtime_source의 lease 재선언이 이 싱크로 나간다, rebind와 동일 순서).
        if let Some(sink) = self.active.runtime.command_sink() {
            server.set_runtime_command_sink(sink);
        }
        server.set_runtime_source(receiver);
        // 구독 등록 직후 전체 워크스페이스 스냅샷을 시드한다 — 이벤트 스트림은 edge-trigger라,
        // 재구독한 대시보드는 과거 이력을 모른다. 시드가 없으면 이미 needs_approval로 정착한
        // 세션이 다음 상태 변화까지 "실행 중"으로 오표시된다(계획 P2 리뷰: 킬러 기능 훼손).
        let seeds = self.web_workspace_seed();
        // 재구독 시점에만 활성 세션의 라이브 상태를 시드한다(매 프레임 push와 분리 —
        // 리뷰 P1-1: 프레임 push가 상태를 덮으면 숨김 창에서 stale 값으로 되돌아간다).
        Self::reseed_web_sessions(&server, &seeds);
        server.set_workspaces(seeds);
        // 폰 미러 진입(I1b-2) — 전환 요청을 egui 스레드 큐로 넘기는 싱크. app 레벨이라 한 번만
        // 주입한다(command_sink처럼 워커별 교체 불필요). 웹 스레드에서 불리므로 큐 push +
        // repaint만 하고, 실제 switch_workspace는 ui()가 큐를 drain해 egui 스레드에서 실행한다.
        let switch_queue = Arc::clone(&self.web_switch_queue);
        let switch_ctx = self.egui_ctx.clone();
        server.set_switch_sink(Arc::new(move |workspace: String| {
            if let Ok(mut queue) = switch_queue.lock() {
                queue.push(workspace);
            }
            switch_ctx.request_repaint();
        }));
        Ok(WebRemoteState { server, token })
    }

    /// 워크스페이스 전환 시 웹 대시보드를 새 활성 worker에 재구독시킨다 — 옛 receiver는
    /// 교체와 함께 drop되어 옛 worker가 자기 subscriber를 정리한다(계획 P2 "wake 클로저 수명").
    fn rebind_web_dashboard(&self) {
        if let Some(web) = &self.web {
            let receiver = self
                .active
                .runtime
                .subscribe_with_wake_background(web.server.dashboard_wake());
            // 명령 싱크를 receiver보다 먼저 교체한다 — set_runtime_source의 lease 재선언이
            // 새 worker의 싱크로 나가게 (P5b, 워크스페이스 전환 시 시청 연속성).
            if let Some(sink) = self.active.runtime.command_sink() {
                web.server.set_runtime_command_sink(sink);
            }
            web.server.set_runtime_source(receiver);
            // 재구독 직후 새 워크스페이스 스냅샷을 시드한다(start_web과 동일 이유 + 세션 맵
            // 통째 교체로 옛 워크스페이스 세션 정체/전환 레이스까지 해소 — P2 리뷰).
            let seeds = self.web_workspace_seed();
            Self::reseed_web_sessions(&web.server, &seeds);
            web.server.set_workspaces(seeds);
        }
    }

    /// 재구독 시점 시딩 — 스냅샷에서 활성 워크스페이스의 세션만 골라 넘긴다.
    fn reseed_web_sessions(
        server: &web_remote::WebRemoteServer,
        seeds: &[web_remote::dashboard::WorkspaceSeed],
    ) {
        if let Some(active) = seeds
            .iter()
            .find(|ws| ws.state == web_remote::dashboard::WorkspaceState::Active)
        {
            server.reseed_active_sessions(&active.sessions);
        }
    }

    /// 웹 대시보드용 **전체 워크스페이스** 스냅샷 — 활성 1개 + warm/유휴 N개.
    /// 데스크톱 활동 패널(activity_rows)과 같은 원천·같은 이름 규칙을 쓴다: 세션 표시명은
    /// 기본 제목이면 프로젝트명으로 해석되고(activity_session_name / resolve_session_title),
    /// 사용자가 rename했으면 그대로다 — 폰에서도 "workspace.spawn.shell 140"이 아니라
    /// 사람이 읽는 이름이 보인다.
    ///
    /// 활성 세션만 id를 싣는다(시청/입력 대상). warm/유휴는 표시 전용 — 세션 id는
    /// worker-로컬이라 다른 워크스페이스 id로 시청하면 엉뚱한 세션이 잡힌다(P5 리뷰 P2).
    ///
    /// 활성 워크스페이스 상태는 런타임 감지 이벤트에서만 오므로(앱의 transcript/hook 병합은
    /// 브리지에 안 보임) 병합 맵은 비워 넘겨 순수 감지 상태를 시드한다 — 브리지의 이벤트
    /// 갱신과 일관된다. 브라우저는 innerHTML 금지라 제목 문자열은 그대로 안전하다.
    fn web_workspace_seed(&self) -> Vec<web_remote::dashboard::WorkspaceSeed> {
        use web_remote::dashboard::{SessionSeed, WorkspaceSeed, WorkspaceState};
        let empty_activity = std::collections::HashMap::new();
        let empty_needs_input = std::collections::HashSet::new();
        let empty_turn_done = std::collections::HashMap::new();
        self.workspaces
            .iter()
            .map(|ws| {
                if ws.id == self.active.id {
                    let sessions = self
                        .active
                        .workspace_ui
                        .session_entries(
                            &self.i18n,
                            &empty_activity,
                            &empty_needs_input,
                            &empty_turn_done,
                        )
                        .into_iter()
                        .filter_map(|entry| {
                            let session = entry.session?;
                            Some(SessionSeed {
                                id: Some(session.0),
                                title: entry.title,
                                status: Some(
                                    entry.status.unwrap_or(runtime::SessionStatus::Running),
                                ),
                                // 돌고 있는 에이전트("Claude · sonnet · high") — 사이드바
                                // 2행과 같은 원천(agent_detect). 셸이면 None.
                                agent: entry.agent_line,
                                exited: self.active.live.exited_sessions.contains(&session),
                            })
                        })
                        .collect();
                    return WorkspaceSeed {
                        id: ws.id.clone(),
                        name: Self::workspace_display_name(ws),
                        state: WorkspaceState::Active,
                        sessions,
                    };
                }
                // 대기(warm)/절전 — 감지 워커가 안 돌아 상태는 없다. 이름만 활동 패널과 동일 규칙.
                // 대기는 에이전트가 살아있어 마지막 감지 정보를 유지·표시하고(방안①), 절전은 죽어
                // 표시하지 않는다.
                let (state, sessions): (WorkspaceState, Vec<SessionSeed>) =
                    match self.warm.get(&ws.id) {
                        Some(rt) => {
                            let mut ids: Vec<_> = rt.session_titles.keys().copied().collect();
                            ids.sort_by_key(|s| s.0);
                            let sessions = ids
                                .iter()
                                .filter_map(|s| {
                                    let raw = rt.session_titles.get(s)?;
                                    Some(SessionSeed {
                                        id: None, // 표시 전용
                                        title: self.activity_session_name(&ws.id, raw),
                                        status: None,
                                        agent: rt.workspace_ui.agent_line_for(*s),
                                        exited: false,
                                    })
                                })
                                .collect();
                            (WorkspaceState::Warm, sessions)
                        }
                        None => {
                            let sessions = self
                                .persisted_activity_panes
                                .get(&ws.id)
                                .into_iter()
                                .flatten()
                                .map(|(title, _cwd)| SessionSeed {
                                    id: None, // 표시 전용
                                    title: self.activity_session_name(&ws.id, title),
                                    status: None,
                                    agent: None, // 절전 — 에이전트 죽음
                                    exited: false,
                                })
                                .collect();
                            (WorkspaceState::Suspended, sessions)
                        }
                    };
                WorkspaceSeed {
                    id: ws.id.clone(),
                    name: Self::workspace_display_name(ws),
                    state,
                    sessions,
                }
            })
            .collect()
    }

    /// 워크스페이스 표시 스냅샷을 웹 대시보드에 반영한다(변화가 없으면 브리지가 무시).
    ///
    /// **상태는 시드하지 않는다** — 활성 세션의 상태는 런타임 이벤트가 소유하는 프레임
    /// 독립 데이터다(리뷰 P1-1). 여기서는 구성·표시명만 보낸다.
    ///
    /// 스냅샷 **구축 비용**(제목 해석의 .git 상향 stat 등)이 프레임마다 들지 않도록
    /// 스로틀한다 — 폰의 ≤1s 반영 요건에 여유가 큰 250ms (리뷰 P2-2).
    fn sync_web_workspaces(&mut self, now: std::time::Instant) {
        const WEB_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
        if self.web.is_none() {
            return;
        }
        if let Some(last) = self.last_web_sync
            && now.duration_since(last) < WEB_SYNC_INTERVAL
        {
            return;
        }
        self.last_web_sync = Some(now);
        let seeds = self.web_workspace_seed();
        if let Some(web) = &self.web {
            web.server.set_workspaces(seeds);
        }
    }

    /// settings 토글 on: 웹서버를 켜고 성공 시 config에 의도를 영속한다 (remote_enable 관례).
    fn web_enable(&mut self) {
        match self.start_web() {
            Ok(state) => {
                self.web = Some(state);
                self.web_error = None;
                // 포트가 바뀌었을 수 있다 — serve 진단을 무효화해 다음 프레임에 재진단한다.
                self.serve_state = None;
                self.config.web.enabled = true;
                if let Err(e) = self.config.save(&self.config_path) {
                    tracing::warn!("config 저장 실패: {e:#}");
                    self.web_error = Some(format!(
                        "설정 저장 실패 — 다음 실행엔 자동시작 안 됨: {e:#}"
                    ));
                }
            }
            Err(e) => {
                tracing::warn!("모바일 웹 서버 시작 실패: {e:#}");
                self.web_error = Some(format!("{e:#}"));
            }
        }
    }

    /// settings 토글 off: 웹서버 정지(Drop이 accept/접속 스레드 정리) + config 영속.
    fn web_disable(&mut self) {
        if let Some(state) = self.web.take() {
            state.server.shutdown();
        }
        self.web_error = None;
        self.serve_state = None; // 서버가 없으면 진단은 의미 없다
        self.config.web.enabled = false;
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::warn!("config 저장 실패: {e:#}");
            // 저장 실패를 조용히 넘기면 껐다고 생각한 서버가 다음 실행에 자동시작된다 — 표면화.
            self.web_error = Some(format!(
                "서버는 껐지만 설정 저장 실패 — 다음 실행에 다시 켜질 수 있습니다: {e:#}"
            ));
        }
    }

    /// 페어링 토큰 재발급 — 기존 페어링(QR/브라우저 저장분) 무효. 서버는 시작 시 토큰을
    /// 고정하므로 실행 중이면 새 토큰으로 재시작해 반영한다.
    fn web_rotate_token(&mut self) {
        if let Err(e) = web_remote::pairing::rotate_token(&self.secret_store) {
            tracing::warn!("페어링 토큰 재발급 실패: {e:#}");
            self.web_error = Some(format!("{e:#}"));
            return;
        }
        self.web_error = None;
        if let Some(state) = self.web.take() {
            state.server.shutdown();
            match self.start_web() {
                Ok(state) => self.web = Some(state),
                Err(e) => {
                    tracing::warn!("토큰 재발급 후 웹서버 재시작 실패: {e:#}");
                    self.web_error = Some(format!("{e:#}"));
                }
            }
        }
    }

    /// known_hosts 파일을 (host, 지문) 목록으로 로드한다 (표시 전용 — 파일 없으면 빈 목록).
    fn load_known_hosts(&self) -> Vec<(String, String)> {
        let path = self.known_hosts_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => parse_known_hosts(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!("known_hosts 읽기 실패: {e:#}");
                Vec::new()
            }
        }
    }

    /// workspace 전환 (워커-per-workspace §14.1 Warm): 현재 활성 workspace는 Warm으로
    /// 내려 워커를 계속 살려 둔다(에이전트 유지). 대상이 warm 풀에 있으면 재사용(즉시 복귀),
    /// 없으면 새로 만든다. warm 풀이 max_warm을 넘으면 가장 오래된 것을 Suspended(shutdown).
    /// 폰(미러 진입 — I1b-2)이 보낸 전환 요청 큐를 비운다. 웹 스레드가 push한 워크스페이스
    /// id를 egui 스레드에서 switch_workspace로 넘긴다 — 대기=재사용/절전=재생성/상한초과=거부를
    /// switch_workspace가 처리하고, 성공 시 rebind_web_dashboard가 폰·데스크탑을 미러시킨다.
    fn drain_web_switch_requests(&mut self) {
        // 하드 미러라 최종 목적지만 의미 있다 — 큐에 쌓인 중간 요청은 버리고 마지막 하나만
        // 처리한다. 안 그러면 A,B,A,B 연타가 한 프레임에 N번의 워커 spawn+config 저장을
        // egui 스레드에서 유발한다(리뷰 P3). Drain은 next_back으로 마지막만 꺼내도 drop 시
        // 범위 전체를 vec에서 제거하므로 큐는 그대로 비워진다(last()의 전체순회 회피).
        let target: Option<String> = match self.web_switch_queue.lock() {
            Ok(mut queue) => queue.drain(..).next_back(),
            Err(_) => return,
        };
        if let Some(ws_id) = target {
            self.handle_web_switch(&ws_id);
        }
    }

    /// 폰이 요청한 워크스페이스로 전환한다(미러 진입). 알 수 없는 id는 무시(방어), 이미
    /// 활성이면 no-op. 상한 초과로 switch_workspace가 거부하면(active 그대로) 폰에 안내를
    /// 띄운다 — 조용한 실패를 막는다.
    fn handle_web_switch(&mut self, ws_id: &str) {
        if ws_id == self.active.id {
            return; // 이미 활성 — 폰은 이미 미러 중.
        }
        let Some(name) = self
            .workspaces
            .iter()
            .find(|ws| ws.id == ws_id)
            .map(Self::workspace_display_name)
        else {
            tracing::warn!(ws = %ws_id, "폰 전환 요청 — 알 수 없는 워크스페이스 무시");
            return;
        };
        // 절전 깨우기(워커 없음 + 복원할 pane 있음) 판정은 전환 전에 — switch가 워커를
        // 만들고 나면 구분이 사라진다 (I1b-3 "복원 중" 안내).
        let wake_from_suspend = !self.warm.contains_key(ws_id)
            && self
                .persisted_activity_panes
                .get(ws_id)
                .is_some_and(|panes| !panes.is_empty());
        self.switch_workspace(ws_id);
        if self.active.id == ws_id {
            if wake_from_suspend {
                // 절전 해제는 워커 생성+RestoreWorkspace+resume까지 몇 초 걸린다 — 그동안
                // 폰이 빈 세션 목록을 보므로 "복원 중" 안내(TTL 자동 해제, 복원이 끝나면
                // 대시보드 프레임이 세션을 채운다).
                let msg = self.i18n.t("workspace.wake.restoring", &[("name", &name)]);
                self.set_web_notice(Some(msg));
                self.egui_ctx.request_repaint_after(Self::WEB_NOTICE_TTL);
            } else {
                // 대기 재사용 — 즉시 미러되므로 직전 안내만 해제.
                self.set_web_notice(None);
            }
        } else if let Some(target) = self.warm_limit_warning.clone() {
            // 상한 초과로 거부됨 — 폰에 안내(데스크탑 모달과 독립, TTL로 자동 해제).
            let msg = self.i18n.t(
                "workspace.warm_limit.body",
                &[
                    ("target", &target),
                    ("limit", &self.config.performance.max_live_warm.to_string()),
                ],
            );
            self.set_web_notice(Some(msg));
            self.egui_ctx.request_repaint_after(Self::WEB_NOTICE_TTL);
        }
    }

    /// 폰 안내 배너를 세팅/해제한다 (I1b-2). 앱 상태와 브리지 프레임을 함께 갱신한다.
    /// **같은 내용을 다시 세팅해도 TTL(set_at)은 유지한다** — 안 그러면 cap-full 버튼 연타가
    /// 매번 타이머를 리셋해 expire_web_notice가 영영 안 돌고, 서버 notice는 Some에 고정되며
    /// 클라는 내용 dedup으로 재표시를 안 해 "재탭했는데 아무 반응 없음"이 된다(리뷰 P3:
    /// 없애려던 silent failure의 재발). now는 내용이 바뀔 때만 새로 찍는다.
    fn set_web_notice(&mut self, notice: Option<String>) {
        self.web_notice = notice.clone().map(|msg| {
            let set_at = match &self.web_notice {
                Some((prev, at)) if *prev == msg => *at,
                _ => std::time::Instant::now(),
            };
            (msg, set_at)
        });
        if let Some(web) = &self.web {
            web.server.set_dashboard_notice(notice);
        }
    }

    /// notice TTL이 지나면 배너를 내린다 (I1b-2 — logic()에서 매 프레임 확인).
    fn expire_web_notice(&mut self) {
        if let Some((_, set_at)) = &self.web_notice
            && set_at.elapsed() >= Self::WEB_NOTICE_TTL
        {
            self.set_web_notice(None);
        }
    }

    fn switch_workspace(&mut self, target_id: &str) {
        if target_id == self.active.id {
            return;
        }
        let live_warm = self
            .warm
            .values()
            .filter(|runtime| runtime.has_live_sessions())
            .count();
        let target_is_live_warm = self
            .warm
            .get(target_id)
            .is_some_and(WorkspaceRuntime::has_live_sessions);
        let projected = projected_live_warm_count(
            live_warm,
            target_is_live_warm,
            self.active.has_live_sessions(),
        );
        if projected > self.config.performance.max_live_warm as usize {
            self.warm_limit_warning = Some(
                self.workspaces
                    .iter()
                    .find(|workspace| workspace.id == target_id)
                    .map(Self::workspace_display_name)
                    .unwrap_or_else(|| target_id.to_owned()),
            );
            self.egui_ctx.request_repaint();
            return;
        }
        // 대상이 background 정리 중이면 먼저 끝낸다 (같은 window 행 경합 방지 — codex 리뷰).
        self.join_pending_shutdown(target_id);

        // 대상 준비: warm 풀에 있으면 재사용, 없으면 새 워커.
        let mut new_active = match self.warm.remove(target_id) {
            Some(rt) => {
                self.warm_order.retain(|id| id != target_id);
                rt
            }
            None => {
                // 새 워커는 SessionId를 1부터 다시 시작한다 — 이 workspace의 옛 워커
                // lifetime에서 남은 알림을 지운다. 안 그러면 재사용된 SessionId의 완료
                // 알림이 옛 항목과 dup으로 취급돼 안 뜬다 (codex 리뷰).
                self.notifications_ui.prune_workspace(target_id);
                Self::make_runtime(
                    &self.config,
                    &self.logs_base,
                    target_id,
                    &self.db_path,
                    &self.redaction,
                    &self.db,
                    &self.egui_ctx,
                )
            }
        };
        // UI 상태는 리셋하지 않는다 — warm 재사용이면 그동안 누적된 pending_events(=lifecycle
        // 이벤트 포함)를 그대로 ui()가 처리해 exit/status 상태를 재구성해야 하고, workspace_ui는
        // 마지막 active 상태 + 아래 Active 재emit(전체 mux 스냅샷)으로 최신화된다. (새 워커는
        // 이미 fresh + RestoreWorkspace라 리셋 불필요.)
        new_active.render_active = true;
        new_active.backgrounded_at = None;
        let _ = new_active
            .runtime
            .send_command(runtime::RuntimeCommand::SetWorkspaceState(
                runtime::WorkspaceRuntimeState::Active,
            ));

        // 현재 활성을 Warm으로 내리고 warm 풀에 보관 (워커·세션 계속 실행).
        let mut old = std::mem::replace(&mut self.active, new_active);
        // 웹 대시보드가 켜져 있으면 새 활성 worker로 재구독한다(전환 후 상태 스트림 유지).
        self.rebind_web_dashboard();
        // agent 감지 워커: 전환 시 epoch을 올려 이전 워크스페이스의 잔여 결과를 폐기하고,
        // 즉시 감지가 새 워크스페이스 기준으로 재시작되게 한다(codex #3).
        self.agent_detect_epoch += 1;
        self.agent_bindings.clear();
        self.agent_activity.clear();
        self.agent_needs_input.clear();
        self.agent_turn_done.clear();
        self.session_alerts.clear();
        self.session_cwds.clear();
        self.workspace_rename_prompt = None; // 워크스페이스 전환 시 옛 rename 제안 폐기
        self.agent_info.clear();
        self.statuslines.clear();
        let _ = old
            .runtime
            .send_command(runtime::RuntimeCommand::SetWorkspaceState(
                runtime::WorkspaceRuntimeState::Warm,
            ));
        old.render_active = false;
        old.backgrounded_at = Some(std::time::Instant::now());
        let old_id = old.id.clone();
        self.warm.insert(old_id.clone(), old);
        self.warm_order.push(old_id.clone());

        // pending 상태 정리 (이전 워커 응답 못 받음, 교차-ws 감사 방지).
        // notifications는 리셋하지 않는다 — (ws, session)로 namespacing돼 전역 센터가
        // 모든 workspace 알림을 유지한다 (background 완료 통지·클릭 이동, codex 리뷰).
        // agent spawn 대기는 버리지 않고 물러난 workspace로 이관 — 응답이 오기 전까지
        // 그 workspace를 live로 취급해 suspend가 새 PTY를 죽이는 창을 막는다 (codex).
        let pending_agents = self.agents_ui.take_pending();
        if let Some(old_rt) = self.warm.get_mut(&old_id) {
            old_rt.pending_agent_spawns += pending_agents;
        }
        self.connectors_ui.clear_invoke();
        // 파일 트리 루트를 새 workspace path로 갱신 (FT-1)
        self.config.ui.last_workspace_id = Some(target_id.to_owned());
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::warn!("마지막 workspace 저장 실패: {e:#}");
        }
        self.refresh_file_tree_root();
        // 새 workspace의 .env → 환경 profile 동기화 + 기본 env 주입 (2026-07-07).
        // 폴링 기준점도 내부에서 새 root 기준으로 다시 잡힌다.
        self.sync_dotenv_env();
        self.egui_ctx.request_repaint();

        self.evict_warm();
        // 새 runtime 생성 또는 warm 축출로 resident 수가 바뀌었을 수 있다. 설정의
        // 전역 예산을 현재 active+warm 전체에 다시 나눠 각 워커에 반영한다.
        self.broadcast_terminal_cache_policy();
    }

    fn cycle_workspace(&mut self, delta: isize) {
        if self.workspaces.len() < 2 {
            return;
        }
        let current = self
            .workspaces
            .iter()
            .position(|workspace| workspace.id == self.active.id)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.workspaces.len() as isize) as usize;
        let target = self.workspaces[next].id.clone();
        self.switch_workspace(&target);
        self.refresh_workspaces();
    }

    fn handle_agent_sessions_request(&mut self, request: ui::agent_sessions::AgentSessionsRequest) {
        use crate::agent_surface::AgentSurfaceId;
        match request {
            ui::agent_sessions::AgentSessionsRequest::FocusPty(AgentSurfaceId::Pty {
                workspace_id,
                pane_id,
                session_id,
            }) => {
                if workspace_id != self.active.id {
                    self.switch_workspace(&workspace_id);
                    self.refresh_workspaces();
                    self.pending_focus = Some((workspace_id, session_id));
                    return;
                }
                let target = runtime::MuxPaneId(pane_id);
                let tab = self
                    .active
                    .workspace_ui
                    .mux()
                    .and_then(|mux| tab_of_agent_target(mux, &target, session_id));
                if let Some(tab) = tab {
                    if self
                        .active
                        .workspace_ui
                        .mux()
                        .and_then(|mux| mux.active_tab.clone())
                        != Some(tab.clone())
                    {
                        let _ = self
                            .active
                            .runtime
                            .send_command(runtime::RuntimeCommand::SelectTab { tab });
                    }
                    let _ = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::FocusPane { pane: target });
                }
            }
            ui::agent_sessions::AgentSessionsRequest::InterruptPty(AgentSurfaceId::Pty {
                workspace_id,
                pane_id,
                session_id,
            }) => {
                let still_matches = workspace_id == self.active.id
                    && self.active.workspace_ui.mux().is_some_and(|mux| {
                        mux.tabs
                            .iter()
                            .flat_map(|tab| &tab.panes)
                            .any(|pane| pane.id.0 == pane_id && pane.session_id == Some(session_id))
                    });
                if still_matches {
                    let _ = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::WriteInput {
                            session: session_id,
                            bytes: vec![0x03],
                        });
                }
            }
            ui::agent_sessions::AgentSessionsRequest::FocusPty(AgentSurfaceId::Structured {
                ..
            })
            | ui::agent_sessions::AgentSessionsRequest::InterruptPty(
                AgentSurfaceId::Structured { .. },
            ) => unreachable!("PTY 요청은 PTY target만 생성한다"),
        }
    }

    /// 설정에 저장된 전역 단축키 한 건을 실행한다. 설정 창에서는 키 녹화와 검색 입력이
    /// 우선이고, 일반 TextEdit 포커스 중에도 문자 편집 단축키를 가로채지 않는다.
    fn handle_configured_shortcut(&mut self, ctx: &egui::Context) {
        if self.settings_open || ctx.text_edit_focused() {
            return;
        }
        let Some(action) = crate::shortcuts::take_triggered_action(ctx, &self.config.shortcuts)
        else {
            return;
        };

        use crate::shortcuts::ShortcutAction as A;
        match action {
            A::ToggleSidebar => {
                self.config.ui.file_tree_enabled = !self.config.ui.file_tree_enabled;
                self.file_tree = self
                    .config
                    .ui
                    .file_tree_enabled
                    .then(|| self.make_file_tree());
                if let Err(error) = self.config.save(&self.config_path) {
                    tracing::warn!("단축키 설정 저장 실패: {error:#}");
                }
            }
            // 알림은 설정 창이 아니라 벨 팝오버를 토글한다 (v3.9 N1) — 승인/응답을
            // 빠르게 처리하는 경로라 통합 설정 창 전체를 열지 않는다. 전체 기록은
            // 팝오버의 「전체 보기」가 설정→알림으로 연결한다.
            A::OpenNotifications => egui::Popup::toggle_id(ctx, Self::inbox_popup_id()),
            A::OpenEnvironment | A::OpenActivity => {
                self.settings_category = match action {
                    A::OpenEnvironment => ui::settings::Category::Environment,
                    A::OpenActivity => ui::settings::Category::Activity,
                    _ => unreachable!(),
                };
                self.settings_open = true;
                self.refresh_workspaces();
            }
            A::OpenAgents => self.handle_agent_shortcut(action, ctx),
            A::NewShell => self.active.workspace_ui.spawn_shell(
                &self.active.runtime,
                self.config.terminal.scrollback_lines as usize,
            ),
            A::ClosePane => self
                .active
                .workspace_ui
                .close_focused_pane(&self.active.runtime),
            A::ScrollToBottom => self
                .active
                .workspace_ui
                .scroll_focused_to_bottom(&self.active.runtime),
            A::SplitVertical | A::SplitHorizontal => {
                let direction = if action == A::SplitVertical {
                    runtime::SplitDirection::Vertical
                } else {
                    runtime::SplitDirection::Horizontal
                };
                self.active.workspace_ui.split_focused_pane(
                    &self.active.runtime,
                    direction,
                    self.config.terminal.scrollback_lines as usize,
                );
            }
            A::FocusNextPane => self
                .active
                .workspace_ui
                .focus_relative_pane(&self.active.runtime, 1),
            A::FocusPreviousPane => self
                .active
                .workspace_ui
                .focus_relative_pane(&self.active.runtime, -1),
            A::NextWorkspace => self.cycle_workspace(1),
            A::PreviousWorkspace => self.cycle_workspace(-1),
            A::IncreaseTerminalFont | A::DecreaseTerminalFont => {
                let delta = if action == A::IncreaseTerminalFont {
                    0.5
                } else {
                    -0.5
                };
                self.config.terminal.font_size =
                    (self.config.terminal.font_size + delta).clamp(8.0, 32.0);
                self.active.workspace_ui.clear_render_caches();
                for runtime in self.warm.values_mut() {
                    runtime.workspace_ui.clear_render_caches();
                }
                if let Err(error) = self.config.save(&self.config_path) {
                    tracing::warn!("터미널 글꼴 크기 저장 실패: {error:#}");
                }
            }
            A::TerminalSearch => self.active.workspace_ui.open_search(),
            A::ClearRenderCaches => {
                self.active.workspace_ui.clear_render_caches();
                for runtime in self.warm.values_mut() {
                    runtime.workspace_ui.clear_render_caches();
                }
            }
            A::PreviousAgent
            | A::NextAgent
            | A::FocusAgentInput
            | A::NewStructuredAgent
            | A::InterruptAgent
            | A::ApproveAgent
            | A::RejectAgent
            | A::IncreaseAgentEffort
            | A::DecreaseAgentEffort => {
                self.handle_agent_shortcut(action, ctx);
            }
        }
        ctx.request_repaint();
    }

    fn handle_agent_shortcut(
        &mut self,
        shortcut: crate::shortcuts::ShortcutAction,
        ctx: &egui::Context,
    ) {
        use crate::agent_actions::{AgentAction, AgentActionGate, gate_action};
        use crate::shortcuts::ShortcutAction as A;

        let action = match shortcut {
            A::OpenAgents => AgentAction::OpenAgents,
            A::PreviousAgent => AgentAction::SelectPrevious,
            A::NextAgent => AgentAction::SelectNext,
            A::FocusAgentInput => AgentAction::FocusInput,
            A::NewStructuredAgent => AgentAction::NewStructured,
            A::InterruptAgent => AgentAction::Interrupt,
            A::ApproveAgent => AgentAction::ApproveOnce,
            A::RejectAgent => AgentAction::Reject,
            A::IncreaseAgentEffort => AgentAction::EffortUp,
            A::DecreaseAgentEffort => AgentAction::EffortDown,
            _ => return,
        };
        let selected = self.agent_sessions_ui.selected_surface_snapshot();
        let pending = self.agent_sessions_ui.selected_pending_approval_count();
        let gate = gate_action(action, selected.as_ref(), pending);
        if !gate.is_allowed() {
            self.agent_sessions_ui.open();
            match gate {
                AgentActionGate::NoTarget => tracing::info!("에이전트 단축키: 선택 없음"),
                AgentActionGate::Unsupported => {
                    tracing::info!("에이전트 단축키: 선택 transport에서 지원하지 않음")
                }
                AgentActionGate::ApprovalCountMismatch { pending } => {
                    tracing::info!(pending, "에이전트 승인 단축키 안전 조건 불충족")
                }
                AgentActionGate::Allowed => unreachable!(),
            }
            return;
        }

        let request = match action {
            AgentAction::SelectPrevious => {
                self.agent_sessions_ui.select_relative(-1);
                None
            }
            AgentAction::SelectNext => {
                self.agent_sessions_ui.select_relative(1);
                None
            }
            AgentAction::FocusInput => self.agent_sessions_ui.focus_selected_input(),
            AgentAction::NewStructured => {
                self.agent_sessions_ui.open_new_prompt();
                None
            }
            AgentAction::Interrupt => match self.agent_sessions_ui.interrupt_selected(ctx) {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!("에이전트 중단 단축키 실패: {error:#}");
                    None
                }
            },
            AgentAction::ApproveOnce => {
                if let Err(error) = self.agent_sessions_ui.approve_selected_once(ctx) {
                    tracing::warn!("에이전트 승인 단축키 실패: {error:#}");
                }
                None
            }
            AgentAction::Reject => {
                if let Err(error) = self.agent_sessions_ui.reject_selected(ctx) {
                    tracing::warn!("에이전트 거절 단축키 실패: {error:#}");
                }
                None
            }
            AgentAction::EffortUp | AgentAction::EffortDown => {
                self.agent_sessions_ui.open();
                let delta = if action == AgentAction::EffortUp {
                    1
                } else {
                    -1
                };
                if let Err(error) = self.agent_sessions_ui.adjust_selected_effort(delta) {
                    tracing::warn!("에이전트 effort 단축키 실패: {error:#}");
                }
                None
            }
            AgentAction::OpenAgents => {
                self.agent_sessions_ui.open();
                None
            }
        };
        if let Some(request) = request {
            self.handle_agent_sessions_request(request);
        }
    }

    /// warm 풀이 max_warm(설정)을 넘으면 가장 오래된 것부터 Suspended로 내린다 (워커
    /// shutdown, 세션 종료 — §14.1 Suspended). background 스레드에서 정리하고 on_exit에서 join.
    /// **live 세션(미종료 셸/에이전트)이 있는 workspace는 축출하지 않는다** — 진행 중
    /// 작업을 경고 없이 kill하지 않기 위해 상한 초과를 허용한다 (메모리 < 작업 보호).
    fn evict_warm(&mut self) {
        let max_warm = self.config.performance.max_warm as usize;
        let evictable = warm_eviction_candidates(&self.warm_order, max_warm, |id| {
            self.warm.get(id).is_some_and(|rt| rt.has_live_sessions())
        });
        for evict_id in evictable {
            self.warm_order.retain(|id| id != &evict_id);
            self.suspend_warm_workspace(&evict_id, false);
        }
    }

    fn evict_idle_warm(&mut self, now: std::time::Instant) {
        let resident_before = 1 + self.warm.len();
        let expired = expired_warm_workspace_ids(
            &self.warm_order,
            |id| self.warm.get(id).and_then(|rt| rt.backgrounded_at),
            now,
            Self::WARM_AUTO_SUSPEND_AFTER,
        );
        for id in expired {
            // 에이전트/자식 작업은 계속 보호한다. 30분 동안 background였고 resource
            // 샘플로 프롬프트 대기 셸 리더만 확인된 경우에만 셸을 재생성 가능한 상태로 내린다.
            if self
                .warm
                .get(&id)
                .is_some_and(|rt| rt.has_live_sessions() && !rt.can_auto_suspend_idle_shells())
            {
                continue;
            }
            self.warm_order.retain(|warm_id| warm_id != &id);
            self.suspend_warm_workspace(&id, true);
        }
        if 1 + self.warm.len() != resident_before {
            self.broadcast_terminal_cache_policy();
        }
    }

    fn suspend_warm_workspace(&mut self, workspace_id: &str, allow_idle_shells: bool) {
        if let Some(mut rt) = self.warm.remove(workspace_id) {
            // 마지막으로 큐에 남은 이벤트를 처리해 방금 끝난 background 작업의 완료/오류
            // 알림을 놓치지 않는다 (codex 리뷰 — 축출 시 receiver drop으로 유실되던 것).
            let events = rt.events.drain();
            if rt.events.take_overflowed() {
                self.runtime_stream_warning = true;
            }
            Self::record_activity_events(&mut rt, &events);
            let agent_providers = rt.workspace_ui.agent_providers();
            Self::process_ws_notifications(
                &mut self.notifications_ui,
                workspace_id,
                &events,
                &mut rt.session_titles,
                &agent_providers,
                &self.i18n,
            );
            // 최종 방어: 마지막 drain에서 새 spawn/자식 작업이 관측됐을 수 있다.
            // 일반 축출은 live를 모두 보호하고, timeout 축출도 안전한 idle 셸 조건을
            // 다시 만족할 때만 진행한다.
            let has_live = rt.has_live_sessions();
            let idle_shells = allow_idle_shells && rt.can_auto_suspend_idle_shells();
            if has_live && !idle_shells {
                tracing::info!(
                    workspace_id,
                    "suspend 취소 — 실행 중 세션이 있어 warm 유지 (작업 보호)"
                );
                // drain한 lifecycle 이벤트를 replay 큐에 보존 — 버리면 재활성 시
                // exit/status 상태가 UI에 재구성되지 않는다 (codex Medium).
                rt.pending_events.extend(events);
                self.warm.insert(workspace_id.to_owned(), rt);
                self.warm_order.push(workspace_id.to_owned());
                return;
            }
            if idle_shells {
                tracing::info!(workspace_id, "30분 유휴 셸 workspace를 suspend");
            }
            // 축출 = Suspended(워커 종료) — 그 workspace의 진행형 알림은 더는 조치
            // 불가하므로 정리한다 (결과 알림은 기록이라 유지, codex 리뷰).
            self.notifications_ui.prune_transient(workspace_id);
            self.pending_shutdowns.retain(|(_, h)| !h.is_finished());
            let evict_id = workspace_id.to_owned();
            let handle = std::thread::spawn(move || {
                let mut runtime = rt.runtime;
                let _ = runtime.send_command(runtime::RuntimeCommand::SetWorkspaceState(
                    runtime::WorkspaceRuntimeState::Suspended,
                ));
                runtime.shutdown();
            });
            self.pending_shutdowns.push((evict_id, handle));
        }
    }

    /// 주어진 workspace의 대기 중 background shutdown들을 join한다 (같은 workspace 워커가
    /// 동시에 두 개 살아 layout 행을 경합하지 않도록). 다른 workspace 것은 남겨 둔다.
    fn join_pending_shutdown(&mut self, workspace_id: &str) {
        let mut i = 0;
        while i < self.pending_shutdowns.len() {
            if self.pending_shutdowns[i].0 == workspace_id {
                let (_, handle) = self.pending_shutdowns.remove(i);
                let _ = handle.join();
            } else {
                i += 1;
            }
        }
    }

    /// 활성 workspace의 `.env`를 환경 profile로 동기화하고, 그 env를 워커 기본 env로
    /// 전송한다(SetSessionDefaultEnv). 파일/SQLite/keyring 작업은 전용 worker에서 수행하고
    /// 이 메서드는 bounded/coalesced 요청만 넣으므로 UI thread를 막지 않는다.
    fn sync_dotenv_env(&mut self) {
        self.request_dotenv_sync(true);
    }

    fn request_dotenv_sync(&mut self, force: bool) {
        if self.dotenv_sync_worker_failed {
            return;
        }
        let workspace_id = self.active.id.clone();
        let root = self.active_tree_root();
        let context = (workspace_id.clone(), root.clone());
        let context_changed = self.dotenv_sync_context.as_ref() != Some(&context);
        if context_changed {
            self.dotenv_sync_generation = self.dotenv_sync_generation.wrapping_add(1);
            self.dotenv_sync_context = Some(context);
            self.last_dotenv_state = None;
            // 이전 context의 대기 요청은 최신 workspace/root 요청으로 교체한다. 이미 실행 중인
            // 결과는 generation 검사에서 폐기된다.
            self.dotenv_sync_deferred = None;
        }
        if force || context_changed {
            self.dotenv_sync_revision = self.dotenv_sync_revision.wrapping_add(1);
        }
        let job = DotenvSyncJob {
            generation: self.dotenv_sync_generation,
            revision: self.dotenv_sync_revision,
            workspace_id,
            root,
            previous_state: self.last_dotenv_state,
            force,
        };
        if self.dotenv_sync_pending {
            if let Some(deferred) = self.dotenv_sync_deferred.as_mut()
                && deferred.generation == job.generation
            {
                deferred.force |= job.force;
                deferred.previous_state = job.previous_state;
                deferred.revision = job.revision;
            } else {
                self.dotenv_sync_deferred = Some(job);
            }
            return;
        }
        self.dispatch_dotenv_sync(job);
    }

    fn dispatch_dotenv_sync(&mut self, mut job: DotenvSyncJob) {
        if job.generation != self.dotenv_sync_generation
            || job.revision != self.dotenv_sync_revision
        {
            return;
        }
        job.previous_state = self.last_dotenv_state;
        match self.dotenv_sync_worker.try_request(job) {
            Ok(()) => self.dotenv_sync_pending = true,
            Err(std::sync::mpsc::TrySendError::Full(job)) => {
                // 정상 경로에서는 pending=true일 때만 찬다. 방어적으로 최신 한 건만 보존한다.
                self.dotenv_sync_deferred = Some(job);
                self.egui_ctx
                    .request_repaint_after(std::time::Duration::from_millis(25));
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.dotenv_sync_worker_failed = true;
                self.dotenv_sync_deferred = None;
                tracing::warn!("dotenv background worker 연결 종료");
            }
        }
    }

    fn poll_dotenv_sync(&mut self) {
        while let Ok(outcome) = self.dotenv_sync_worker.rx.try_recv() {
            self.dotenv_sync_pending = false;
            let current = self.dotenv_sync_context.as_ref().is_some_and(|context| {
                outcome.generation == self.dotenv_sync_generation
                    && outcome.revision == self.dotenv_sync_revision
                    && context.0 == outcome.workspace_id
                    && context.1 == outcome.root
                    && self.active.id == outcome.workspace_id
            });
            if current {
                self.last_dotenv_state = Some(outcome.baseline);
                let mut restore_ready = true;
                match outcome.result {
                    Ok(None) => {
                        // 첫 복원 요청은 force라 보통 도달하지 않는다. worker 재시작/호출 순서가
                        // 달라져도 stale 기본 env 없이 복원하도록 빈 값 명령을 선행한다.
                        if self.active.restore_pending_since.is_some() {
                            restore_ready = self
                                .active
                                .runtime
                                .send_command(runtime::RuntimeCommand::SetSessionDefaultEnv {
                                    env_plain: Vec::new(),
                                    env_secrets: Vec::new(),
                                })
                                .is_ok();
                        }
                    }
                    Ok(Some(payload)) => {
                        if let Some(report) = payload.report
                            && report.upserted + report.removed > 0
                        {
                            tracing::info!(
                                upserted = report.upserted,
                                removed = report.removed,
                                ".env → 환경 profile 동기화"
                            );
                        }
                        // .env 라이브 반영(E5 ⑨) 활성 조건 — 새 셸의 precmd 훅이 이
                        // 두 값으로 깨어난다. 토글/경로 변경이 다음 동기화에 반영된다.
                        let mut env_plain = payload.env_plain;
                        if self.config.ui.env_live_reload
                            && let Some(root) = outcome.root.as_deref()
                        {
                            env_plain.push(("DEPPY_ENV_LIVE_RELOAD".to_owned(), "1".to_owned()));
                            env_plain.push((
                                "DEPPY_PROJECT_ROOT".to_owned(),
                                root.display().to_string(),
                            ));
                        }
                        restore_ready = self
                            .active
                            .runtime
                            .send_command(runtime::RuntimeCommand::SetSessionDefaultEnv {
                                env_plain,
                                env_secrets: payload.env_secrets,
                            })
                            .is_ok();
                        self.invalidate_env_profile_ui();
                        self.credentials_ui.invalidate_cache();
                        self.invalidate_env_api_projects();
                    }
                    Err(error) => {
                        tracing::warn!(".env background 동기화 실패: {error:#}");
                        // transient I/O/keyring/DB 오류는 다음 2초 점검에서 반드시 재시도한다.
                        self.last_dotenv_state = None;
                        // 읽지 못한 secret을 이전 workspace/default env에서 계속 주입하는 것보다
                        // 새 셸의 기본 env를 비우는 쪽이 보안상 안전하다.
                        restore_ready = self
                            .active
                            .runtime
                            .send_command(runtime::RuntimeCommand::SetSessionDefaultEnv {
                                env_plain: Vec::new(),
                                env_secrets: Vec::new(),
                            })
                            .is_ok();
                        self.invalidate_env_profile_ui();
                        self.credentials_ui.invalidate_cache();
                        self.invalidate_env_api_projects();
                    }
                }
                if !restore_ready {
                    // runtime command queue가 잠시 찼다면 동일 baseline을 완료로 확정하지
                    // 않는다. 다음 점검이 기본 env 전송을 다시 시도한다.
                    self.last_dotenv_state = None;
                }
                if restore_ready {
                    self.complete_active_restore();
                }
            }
        }
        if !self.dotenv_sync_pending
            && let Some(job) = self.dotenv_sync_deferred.take()
        {
            self.dispatch_dotenv_sync(job);
        }
    }

    fn complete_active_restore(&mut self) {
        if self.active.restore_pending_since.is_none() {
            return;
        }
        // 복원 전에 캐시 정책부터 — 복원된 exited 세션들이 첫 tick에 설정값 기준으로
        // archive되도록 (§14.3 확장).
        let _ = self
            .active
            .runtime
            .send_command(self.terminal_cache_policy_command());
        match self
            .active
            .runtime
            .send_command(runtime::RuntimeCommand::RestoreWorkspace)
        {
            Ok(()) => self.active.restore_pending_since = None,
            Err(error) => tracing::warn!("workspace 복원 명령 전송 지연: {error:#}"),
        }
    }

    /// 설정의 exited cap / **프로세스 전역** 캐시 예산을 워커 정책 명령으로 만든다.
    /// 각 runtime은 자기 세션만 볼 수 있으므로 active+warm resident 수로 균등 분배해
    /// 합산 허용량이 설정값을 넘지 않게 한다(§14.3 확장).
    fn terminal_cache_policy_command(&self) -> runtime::RuntimeCommand {
        runtime::RuntimeCommand::SetTerminalCachePolicy {
            max_exited_backends: self.config.terminal.exited_backend_cap as usize,
            cache_budget_bytes: per_runtime_cache_budget_bytes(
                self.config.terminal.cache_budget_mb,
                1 + self.warm.len(),
            ),
        }
    }

    /// 캐시 정책을 활성 + warm 워커 전체에 반영한다 (설정 또는 resident 수 변경 시).
    fn broadcast_terminal_cache_policy(&mut self) {
        let command = self.terminal_cache_policy_command();
        let _ = self.active.runtime.send_command(command.clone());
        for rt in self.warm.values() {
            let _ = rt.runtime.send_command(command.clone());
        }
    }

    fn poll_restore_timeout(&mut self) {
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        let Some(started) = self.active.restore_pending_since else {
            return;
        };
        let elapsed = started.elapsed();
        if elapsed < TIMEOUT {
            self.egui_ctx.request_repaint_after(TIMEOUT - elapsed);
            return;
        }
        // 외장 볼륨/keychain이 멈춰도 앱 복원이 영구 대기하지 않는다. 빈 기본 env가 먼저
        // 들어간 경우에만 Restore를 보내며, 늦게 도착한 동기화 결과는 이후 새 셸에 적용된다.
        if self
            .active
            .runtime
            .send_command(runtime::RuntimeCommand::SetSessionDefaultEnv {
                env_plain: Vec::new(),
                env_secrets: Vec::new(),
            })
            .is_ok()
        {
            tracing::warn!("dotenv background 동기화 timeout — 빈 env로 workspace 복원");
            self.complete_active_restore();
        }
    }

    /// 폴더의 (dev, ino)를 읽는다(inode 앵커용). 유효 디렉터리가 아니면 None.
    fn folder_anchor(path: &str) -> Option<(i64, i64)> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(path).ok()?;
            if !meta.is_dir() {
                return None;
            }
            Some((meta.dev() as i64, meta.ino() as i64))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    }

    /// 활성 workspace의 프로젝트 폴더 앵커(dev,ino)를 현재 경로 기준으로 저장한다.
    fn save_workspace_anchor(&self) {
        let anchor = self
            .db
            .workspace_path(&self.active.id)
            .ok()
            .flatten()
            .filter(|p| !p.trim().is_empty())
            .and_then(|p| Self::folder_anchor(&p));
        let _ =
            self.db
                .set_workspace_anchor(&self.active.id, anchor.map(|a| a.0), anchor.map(|a| a.1));
    }

    /// 프로젝트 폴더 rename/이동 감지(2s 폴링). 저장된 경로가 stale(사라짐)이고, 세션 cwd 중
    /// 저장된 앵커(dev,ino)와 일치하는 폴더가 있으면 → 그 폴더가 이동된 새 경로다. 확인 모달로
    /// 제안한다(사용자 요청 2026-07-08). 앵커 없으면(구 워크스페이스) 유효 경로일 때 backfill.
    fn detect_workspace_folder_rename(&mut self) {
        if self.workspace_rename_prompt.is_some() {
            return; // 이미 확인 대기 중
        }
        let ws = self.active.id.clone();
        let Some(path) = self
            .db
            .workspace_path(&ws)
            .ok()
            .flatten()
            .filter(|p| !p.trim().is_empty())
        else {
            return; // 폴더 미설정 — 감지 대상 아님
        };
        let anchor = self.db.workspace_anchor(&ws).ok().flatten();
        let current = Self::folder_anchor(&path); // path가 유효 디렉터리면 그 (dev,ino)
        // 정상 상태 판정: path가 유효하고 앵커가 없거나(backfill) 앵커와 inode가 일치.
        if let Some(cur) = current {
            match anchor {
                None => self.save_workspace_anchor(), // 구 워크스페이스 backfill
                Some(a) if a == cur => {}             // 정상 — 같은 폴더
                Some(_) => {
                    // path는 유효하지만 inode가 다르다 = 원래 폴더가 이동되고 그 자리에 다른
                    // 폴더가 들어섰다(mv proj proj.old && mkdir proj). stale로 보고 매칭 진행.
                    self.propose_rename_by_anchor(&ws, &path, anchor);
                    return;
                }
            }
            self.dismissed_renames.remove(&ws); // 경로 정상화 → 무시 상태 해제
            return;
        }
        // path가 사라짐(stale) → 앵커로 이동 위치 탐색.
        self.propose_rename_by_anchor(&ws, &path, anchor);
    }

    /// 저장 앵커(dev,ino)와 일치하는 세션 cwd를 찾아 rename 복구 모달을 제안한다.
    fn propose_rename_by_anchor(&mut self, ws: &str, old_path: &str, anchor: Option<(i64, i64)>) {
        if self.dismissed_renames.contains(ws) {
            return;
        }
        let Some(anchor) = anchor else {
            return; // 앵커 없음 → 자동 복구 불가(재선택 안내는 파일트리/환경메뉴가 담당)
        };
        for cwd in self.session_cwds.values() {
            // 후보는 현재 저장 경로와 달라야 한다(같으면 이동 아님).
            if cwd != old_path && Self::folder_anchor(cwd) == Some(anchor) {
                self.workspace_rename_prompt = Some((old_path.to_owned(), cwd.clone()));
                return;
            }
        }
    }

    /// .env (존재여부, mtime)을 2초 간격으로 폴링해 변화 시 재동기화한다 — 사이드바 OFF로
    /// 워처가 없을 때의 fallback(codex). stat도 worker에서 수행하고, 변화가 없으면 DB/keyring
    /// 작업을 생략한다. 워처 경로와 중복 실행돼도 요청은 최신 한 건으로 축약된다.
    fn poll_dotenv_change(&mut self) {
        if self.last_dotenv_check.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        self.last_dotenv_check = std::time::Instant::now();
        // 프로젝트 폴더 rename/이동 감지도 같은 2s 주기로 (앵커 backfill 포함).
        self.detect_workspace_folder_rename();
        self.request_dotenv_sync(false);
    }

    /// 활성 workspace의 트리 루트 (path 미설정/조회 실패 → None → 안내 표시 §9-2).
    fn active_tree_root(&self) -> Option<PathBuf> {
        match self.db.workspace_path(&self.active.id) {
            Ok(path) => Self::workspace_path_to_tree_root(path),
            Err(e) => {
                tracing::warn!("workspace 경로 조회 실패: {e:#}");
                None
            }
        }
    }

    /// T1: focused pane 세션의 현재 작업 폴더 — agent_detect 워커(lsof)가 채운
    /// `session_cwds`를 재사용한다 (새 감지 메커니즘 없음).
    fn focused_session_cwd(&self) -> Option<String> {
        self.active
            .workspace_ui
            .focused_session()
            .and_then(|sid| self.session_cwds.get(&sid))
            .cloned()
    }

    /// T1: pane 우클릭 → 환경설정 진입 시점에 focused 세션 cwd를 감지해 배너 상태를
    /// 만든다. cwd가 없거나 폴더가 아니면 None. 비교는 canonicalize 기준
    /// (macOS `/var`↔`/private/var`, 심링크 등)으로 하되 실패 시 원경로로 폴백.
    fn detect_session_cwd_banner(&self) -> Option<EnvSessionCwdBanner> {
        let cwd = std::path::PathBuf::from(self.focused_session_cwd()?);
        if !cwd.is_dir() {
            return None;
        }
        let cwd_canon = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
        let roots: Vec<std::path::PathBuf> = self
            .workspaces
            .iter()
            .filter(|ws| !ws.path.trim().is_empty())
            .map(|ws| {
                let p = std::path::PathBuf::from(ws.path.trim());
                std::fs::canonicalize(&p).unwrap_or(p)
            })
            .collect();
        Some(EnvSessionCwdBanner {
            registered: cwd_belongs_to_any(&cwd_canon, &roots),
            cwd,
        })
    }

    /// 워크스페이스 표시 이름 — 포커스 세션의 현재 작업 폴더(git 저장소면 프로젝트명)를
    /// name에 자동 저장하고 그걸 표시한다(2026-07-08). 아직 감지 전이면 path 폴더명,
    /// 그것도 없으면 "~". 자동 추적이라 포커스 이동·재시작에도 마지막 폴더가 유지된다.
    /// 워크스페이스 표시 이름 (E3, 2026-07-13): **프로젝트 폴더명에서 파생**하고
    /// 사용자 이름(name 컬럼 = 별칭)은 `폴더명 (별칭)`으로 병기한다. 정체성이 항상
    /// 실제 폴더에 고정되어, 이름과 경로가 어긋난 워크스페이스에 환경변수를 넣는
    /// 사고(binjari)가 표시 차원에서 재발하지 않는다. 기존에 폴더명과 다른 이름을
    /// 저장한 워크스페이스는 그 이름이 자동으로 별칭으로 강등된다(데이터 무변경).
    fn workspace_display_name(row: &crate::storage::WorkspaceRow) -> String {
        let alias = row.name.trim();
        let alias = (!alias.is_empty() && alias != "default").then_some(alias);
        let folder = {
            let path = row.path.trim();
            (!path.is_empty())
                .then(|| std::path::Path::new(path).file_name())
                .flatten()
                .map(|base| base.to_string_lossy().into_owned())
        };
        match (folder, alias) {
            (Some(folder), Some(alias)) if alias != folder => format!("{folder} ({alias})"),
            (Some(folder), _) => folder,
            (None, Some(alias)) => alias.to_owned(),
            (None, None) => "~".to_owned(),
        }
    }

    fn invalidate_env_api_projects(&mut self) {
        self.env_api_projects_cache = None;
        self.env_project_rows_generation = self.env_project_rows_generation.wrapping_add(1);
        self.env_project_rows_pending = false;
        self.env_project_rows_failed = false;
    }

    fn invalidate_env_profile_ui(&mut self) {
        self.env_profiles_ui.invalidate_cache();
        self.agents_ui.invalidate_profiles_cache();
        self.credentials_ui.clear_revealed_secrets();
        self.env_secret_generation = self.env_secret_generation.wrapping_add(1);
        self.env_secret_cache.clear();
        self.env_secret_pending.clear();
        self.env_secret_failures.clear();
    }

    fn poll_env_secret_reveals(&mut self) {
        while let Ok(outcome) = self.env_secret_reveal_worker.rx.try_recv() {
            if outcome.generation != self.env_secret_generation {
                continue;
            }
            self.env_secret_pending.remove(&outcome.credential_id);
            match outcome.value {
                Ok(value) => {
                    self.env_secret_failures.remove(&outcome.credential_id);
                    self.env_secret_cache.insert(outcome.credential_id, value);
                }
                Err(error) => {
                    tracing::warn!(
                        credential_id = outcome.credential_id,
                        "환경 secret background 조회 실패: {error:#}"
                    );
                    self.env_secret_failures.insert(outcome.credential_id);
                }
            }
        }
    }

    /// env/API 프로젝트 행 캐시 TTL — 설정 UI 안에서의 직접 편집(env var/credential
    /// 추가·삭제)은 하위 UI 내부 상태라 App이 즉시 알 수 없으므로 1초 주기 재계산으로
    /// 반영한다. 집계/경로 stat은 전용 worker에서 수행해 UI thread를 막지 않는다.
    const ENV_API_PROJECTS_TTL: std::time::Duration = std::time::Duration::from_secs(1);

    /// env/API 프로젝트 행 캐시 재계산 필요 판정 (캐시 없음 또는 TTL 경과). 테스트용 분리.
    fn env_api_cache_expired(
        computed_at: Option<std::time::Instant>,
        now: std::time::Instant,
    ) -> bool {
        computed_at.is_none_or(|at| now.saturating_duration_since(at) >= Self::ENV_API_PROJECTS_TTL)
    }

    /// 캐시를 거쳐 env/API 프로젝트 행을 돌려준다. 명시 무효화(refresh_workspaces /
    /// sync_dotenv_env)로 캐시가 비었거나 TTL이 지났으면 worker에 최신 snapshot을 요청하고,
    /// generation이 맞는 결과만 적용한다.
    fn env_api_project_rows_cached(&mut self) -> Vec<ui::env_project_list::EnvProjectRow> {
        let now = std::time::Instant::now();
        while let Ok(outcome) = self.env_project_rows_worker.rx.try_recv() {
            if outcome.generation != self.env_project_rows_generation {
                continue;
            }
            self.env_project_rows_pending = false;
            match outcome.rows {
                Ok(rows) => {
                    self.env_project_rows_failed = false;
                    self.env_api_projects_cache = Some((rows, now));
                }
                Err(error) => {
                    tracing::warn!("환경 프로젝트 목록 background 조회 실패: {error:#}");
                    self.env_project_rows_failed = true;
                    if let Some((_, computed_at)) = &mut self.env_api_projects_cache {
                        *computed_at = now;
                    } else {
                        self.env_api_projects_cache = Some((Vec::new(), now));
                    }
                }
            }
        }
        if Self::env_api_cache_expired(self.env_api_projects_cache.as_ref().map(|(_, at)| *at), now)
            && !self.env_project_rows_pending
        {
            let generation = self.env_project_rows_generation;
            match self.env_project_rows_worker.try_request(EnvProjectRowsJob {
                generation,
                workspaces: self.workspaces.clone(),
            }) {
                Ok(()) => {
                    self.env_project_rows_pending = true;
                    self.env_project_rows_failed = false;
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    self.egui_ctx
                        .request_repaint_after(std::time::Duration::from_millis(25));
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    self.env_project_rows_failed = true;
                }
            }
        }
        self.env_api_projects_cache
            .as_ref()
            .map(|(rows, _)| rows.clone())
            .unwrap_or_else(|| {
                // 첫 background 결과 전에도 목록 골격은 즉시 보인다. count/path 상태만
                // worker 결과에서 채워진다(UI thread filesystem/DB 접근 없음).
                self.workspaces
                    .iter()
                    .map(|workspace| ui::env_project_list::EnvProjectRow {
                        id: workspace.id.clone(),
                        name: Self::workspace_display_name(workspace),
                        alias: workspace.name.clone(),
                        path: workspace.path.clone(),
                        path_missing: false,
                        env_count: 0,
                        key_count: 0,
                    })
                    .collect()
            })
    }

    /// 포커스 세션 cwd → 워크스페이스 이름(현재 작업 폴더/프로젝트명)을 갱신·영속한다.
    /// 변경 시에만 DB에 쓴다(churn 방지). 감지 실패(빈 이름)면 이전 값을 유지한다 —
    /// 포커스가 다른 pane으로 옮겨가도 폴더명이 "~"로 리셋되지 않게(사용자 요청).
    fn update_workspace_folder_name(&mut self, cwd: &str) {
        // E3: 표시 이름은 프로젝트 폴더명에서 파생되고 name 컬럼은 사용자 별칭이다.
        // cwd 자동 추적은 **경로 미지정 + 별칭 없음** 워크스페이스의 부트스트랩에만
        // 남긴다 — 경로가 지정된 뒤 자동 추적이 별칭을 덮어쓰지 않게(별칭 보호).
        let keep = self
            .workspaces
            .iter()
            .find(|w| w.id == self.active.id)
            .is_none_or(|w| {
                !w.path.trim().is_empty()
                    || (!w.name.trim().is_empty() && w.name.trim() != "default")
            });
        if keep {
            return;
        }
        let Some(name) =
            crate::agent_detect::project_display_name(cwd, self.config.ui.session_name_style)
        else {
            return;
        };
        if name.is_empty() || name == "default" {
            return;
        }
        if let Err(e) = self.db.rename_workspace(&self.active.id, &name) {
            tracing::warn!("워크스페이스 폴더명 저장 실패: {e:#}");
            return;
        }
        self.refresh_workspaces();
    }

    fn workspace_path_to_tree_root(path: Option<String>) -> Option<PathBuf> {
        path.and_then(|path| (!path.trim().is_empty()).then(|| PathBuf::from(path)))
    }

    /// 활성 workspace 기준으로 파일 트리 상태를 새로 만든다 (ON 전환/루트 변경 시).
    fn make_file_tree(&self) -> ui::file_tree::FileTreeUi {
        let mut tree = ui::file_tree::FileTreeUi::new(self.egui_ctx.clone());
        // 앱 자신의 data dir(로그·DB·cert 등) 이벤트는 무시 — 로그 쓰기가 워처로 돌아와
        // 리페인트를 유발하는 자기-루프 차단 (workspace 루트가 홈 등 넓은 경로일 때).
        if let Some(data_dir) = self.db_path.parent() {
            tree.set_watch_ignore(vec![data_dir.to_path_buf()]);
        }
        // 워크스페이스 폴더 미설정이어도 파일트리는 **항상** 뜨게 — HOME으로 폴백(사용자
        // 2026-07-08). .env 동기화는 active_tree_root(폴더 미설정=None)를 따로 쓰므로
        // ~/.env를 자동 로드하진 않는다(트리 표시 루트와 .env 원천 분리).
        // 저장 경로가 존재하지 않으면(폴더 이동/삭제/최초 실행 stale) 에러 대신 HOME으로
        // 폴백 — "폴더 못 찾음" 에러가 뜨지 않게(사용자 2026-07-08). rename 감지는 별도.
        tree.set_root(
            self.active_tree_root()
                .filter(|p| p.is_dir())
                .or_else(crate::paths::home_dir),
        );
        tree
    }

    /// 활성 workspace의 트리 루트가 바뀌었을 수 있을 때 (전환/경로 저장) 트리를 재구성한다.
    fn refresh_file_tree_root(&mut self) {
        if self.file_tree.is_some() {
            self.file_tree = Some(self.make_file_tree());
        }
    }

    fn refresh_workspaces(&mut self) {
        match self.db.list_workspaces() {
            Ok(list) => self.workspaces = list,
            Err(e) => tracing::warn!("workspace 목록 조회 실패: {e:#}"),
        }
        let mut structured_threads = Vec::new();
        for workspace in &self.workspaces {
            match self.db.list_structured_threads(&workspace.id, false) {
                Ok(mut rows) => structured_threads.append(&mut rows),
                Err(error) => tracing::warn!(
                    workspace_id = %workspace.id,
                    "구조화 Codex thread 목록 복구 실패: {error:#}"
                ),
            }
        }
        self.agent_sessions_ui
            .import_persisted_threads(structured_threads);
        match self.db.list_persisted_activity_panes() {
            Ok(rows) => {
                let mut by_workspace: std::collections::HashMap<String, Vec<(String, String)>> =
                    std::collections::HashMap::new();
                for (workspace_id, title, cwd) in rows {
                    by_workspace
                        .entry(workspace_id)
                        .or_default()
                        .push((title, cwd));
                }
                self.persisted_activity_panes = by_workspace;
            }
            Err(e) => tracing::warn!("활동 pane snapshot 조회 실패: {e:#}"),
        }
        // 워크스페이스 목록/이름/경로가 바뀌었을 수 있다 — env/API 프로젝트 행 캐시 무효화.
        self.invalidate_env_api_projects();
    }

    /// pressure 뱃지 표시 TTL — 회복 이벤트가 없어(큐가 빠져도 신호 없음) 마지막 관측이
    /// 이 시간보다 오래되면 해소된 것으로 보고 숨긴다. 종료성 사유는 SessionExited가 정리.
    const PRESSURE_TTL: std::time::Duration = std::time::Duration::from_secs(10);

    fn fresh_pressure(
        entry: Option<&(runtime::PtyInputPressure, std::time::Instant)>,
        now: std::time::Instant,
    ) -> Option<runtime::PtyInputPressure> {
        entry
            .filter(|(_, at)| now.saturating_duration_since(*at) < Self::PRESSURE_TTL)
            .map(|(p, _)| p.clone())
    }

    /// 비활성(warm/유휴) 워크스페이스의 pane 표시명 — 활성 워크스페이스의
    /// `resolve_session_title`과 같은 규칙: 사용자가 rename했으면 그대로, 기본 제목
    /// ("셸 N")이면 세션 cwd의 프로젝트명으로 대체한다. 감지 워커는 활성 워크스페이스만
    /// 돌지만 cwd는 worker가 DB에 영속하므로(UpdateSessionCwd) 여기서 재사용한다.
    /// cwd를 못 찾으면 기본 제목을 i18n 렌더한 값("셸 1")으로 폴백.
    fn activity_session_name(&self, workspace_id: &str, raw_title: &str) -> String {
        let cwd = self
            .persisted_activity_panes
            .get(workspace_id)
            .and_then(|panes| pane_cwd(panes, raw_title));
        activity_session_name(raw_title, cwd, &self.i18n, |cwd| {
            self.cached_project_name(cwd)
        })
    }

    /// cwd의 프로젝트 표시명 (메모이즈 — Repo 스타일의 .git 상향 stat이 프레임마다
    /// 반복되지 않게). 키에 현재 스타일을 포함해 설정 전환이 즉시 반영된다.
    fn cached_project_name(&self, cwd: &str) -> Option<String> {
        let style = self.config.ui.session_name_style;
        let key = (style, cwd.to_owned());
        if let Some(hit) = self.project_name_cache.borrow().get(&key) {
            return hit.clone();
        }
        let name = crate::agent_detect::project_display_name(cwd, style)
            .filter(|name| !name.trim().is_empty());
        self.project_name_cache
            .borrow_mut()
            .insert(key, name.clone());
        name
    }

    fn activity_rows(&self) -> Vec<ui::activity::ActivityWorkspaceRow> {
        let now = std::time::Instant::now();
        self.workspaces
            .iter()
            .map(|ws| {
                if ws.id == self.active.id {
                    // pane별 서브행 — 사이드바 3줄 행과 같은 원천(session_entries)에
                    // 세션별 자원/입력압력을 붙인다(2026-07-08).
                    let entries = self.active.workspace_ui.session_entries(
                        &self.i18n,
                        &self.agent_activity,
                        &self.agent_needs_input,
                        &self.agent_turn_done,
                    );
                    let sessions = entries
                        .iter()
                        .map(|e| ui::activity::ActivitySessionRow {
                            name: e.title.clone(),
                            agent_line: e.agent_line.clone(),
                            status_line: e.status_line.clone(),
                            resource: e.session.and_then(|s| {
                                self.active
                                    .session_resource_usage
                                    .iter()
                                    .find(|u| u.session == s)
                                    .cloned()
                            }),
                            pressure: Self::fresh_pressure(
                                e.session
                                    .and_then(|s| self.active.session_input_pressure.get(&s)),
                                now,
                            ),
                        })
                        .collect::<Vec<_>>();
                    return ui::activity::ActivityWorkspaceRow {
                        name: Self::workspace_display_name(ws),
                        state: ui::activity::ActivityWorkspaceState::Active,
                        session_count: entries.len(),
                        pending_events: self.active.pending_events.len(),
                        input_pressure: Self::fresh_pressure(
                            self.active.input_pressure.as_ref(),
                            now,
                        ),
                        backgrounded_for_secs: None,
                        auto_suspend_remaining_secs: None,
                        resource: self.active.resource_usage,
                        session_resources: self.active.session_resource_usage.clone(),
                        sessions,
                    };
                }
                if let Some(rt) = self.warm.get(&ws.id) {
                    let elapsed = rt
                        .backgrounded_at
                        .map(|at| now.saturating_duration_since(at));
                    let auto_suspend_eligible =
                        !rt.has_live_sessions() || rt.can_auto_suspend_idle_shells();
                    let remaining = elapsed.filter(|_| auto_suspend_eligible).map(|duration| {
                        Self::WARM_AUTO_SUSPEND_AFTER
                            .as_secs()
                            .saturating_sub(duration.as_secs())
                    });
                    // warm은 감지 워커가 안 돌아 제목·자원·압력만 채운다 (id 순 정렬).
                    let mut ids: Vec<_> = rt.session_titles.keys().copied().collect();
                    ids.sort_by_key(|s| s.0);
                    let sessions = ids
                        .iter()
                        .map(|s| ui::activity::ActivitySessionRow {
                            // 기본 제목이면 프로젝트명으로 표시 (활성 워크스페이스와 동일 규칙).
                            name: rt
                                .session_titles
                                .get(s)
                                .map(|raw| self.activity_session_name(&ws.id, raw))
                                .unwrap_or_default(),
                            // 대기(warm)는 에이전트가 살아있음 — 활성일 때 감지한 마지막 에이전트
                            // 줄을 유지해 보여준다(방안①). 셸이면 None.
                            agent_line: rt.workspace_ui.agent_line_for(*s),
                            status_line: None,
                            resource: rt
                                .session_resource_usage
                                .iter()
                                .find(|u| u.session == *s)
                                .cloned(),
                            pressure: Self::fresh_pressure(rt.session_input_pressure.get(s), now),
                        })
                        .collect::<Vec<_>>();
                    return ui::activity::ActivityWorkspaceRow {
                        name: Self::workspace_display_name(ws),
                        state: ui::activity::ActivityWorkspaceState::Warm,
                        session_count: rt.session_titles.len(),
                        pending_events: rt.pending_events.len(),
                        input_pressure: Self::fresh_pressure(rt.input_pressure.as_ref(), now),
                        backgrounded_for_secs: elapsed.map(|duration| duration.as_secs()),
                        auto_suspend_remaining_secs: remaining,
                        resource: rt.resource_usage,
                        session_resources: rt.session_resource_usage.clone(),
                        sessions,
                    };
                }
                let sessions = self
                    .persisted_activity_panes
                    .get(&ws.id)
                    .into_iter()
                    .flatten()
                    .map(|(title, _cwd)| ui::activity::ActivitySessionRow {
                        // 유휴 워크스페이스도 프로젝트명으로 표시 (활성/warm과 동일 규칙).
                        name: self.activity_session_name(&ws.id, title),
                        agent_line: None,
                        status_line: None,
                        resource: None,
                        pressure: None,
                    })
                    .collect::<Vec<_>>();
                ui::activity::ActivityWorkspaceRow {
                    name: Self::workspace_display_name(ws),
                    // DB에는 있으나 active/warm runtime이 없는 워크스페이스도 숨기지 않고
                    // 유휴 카드로 표시한다. 현재 복원 레이아웃의 pane은 위 snapshot에서
                    // 하위 세션 행으로 복구한다.
                    state: ui::activity::ActivityWorkspaceState::Idle,
                    session_count: sessions.len(),
                    pending_events: 0,
                    input_pressure: None,
                    backgrounded_for_secs: None,
                    auto_suspend_remaining_secs: None,
                    resource: None,
                    session_resources: Vec::new(),
                    sessions,
                }
            })
            .collect()
    }

    /// 한 workspace의 이벤트에서 제목을 누적(session_titles)하고 상태/exit을 알림으로
    /// 만든다. 알림은 (workspace_id, SessionId)로 식별 — 워커마다 SessionId가 리셋돼
    /// 충돌하므로. 활성/warm 워커 모두 이걸 거쳐 background workspace 알림도 뜬다.
    fn process_ws_notifications(
        notifications: &mut ui::notifications::NotificationsUi,
        workspace_id: &str,
        events: &[runtime::RuntimeEvent],
        session_titles: &mut std::collections::HashMap<runtime::SessionId, String>,
        agent_providers: &std::collections::HashMap<
            runtime::SessionId,
            crate::agent_surface::AgentProvider,
        >,
        catalog: &i18n::Catalog,
    ) {
        for event in events {
            match event {
                runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                    let present: std::collections::HashSet<runtime::SessionId> = snapshot
                        .tabs
                        .iter()
                        .flat_map(|tab| &tab.panes)
                        .filter_map(|pane| pane.session_id)
                        .collect();
                    session_titles.retain(|session, _| present.contains(session));
                    for pane in snapshot.tabs.iter().flat_map(|tab| &tab.panes) {
                        if let Some(session) = pane.session_id {
                            // **raw** 제목을 저장한다 — 표시 시점에 해석한다(활동 패널/폰은
                            // 프로젝트명 규칙, 알림은 i18n 렌더). 렌더된 값을 넣으면
                            // DB의 raw 제목과 매칭되지 않아 프로젝트명 해석이 조용히
                            // 실패한다 (리뷰 P2-1).
                            session_titles.insert(session, pane.title.clone());
                        }
                    }
                }
                runtime::RuntimeEvent::SessionStatusChanged { session, status } => {
                    if let Some(raw) = session_titles.get(session).cloned() {
                        let title = ui::workspace::display_pane_title(&raw, catalog);
                        notifications.on_pty_status(
                            workspace_id,
                            *session,
                            *status,
                            &title,
                            agent_providers.get(session).copied(),
                            catalog,
                        );
                    }
                }
                // regex 없는 agent는 결과가 SessionExited로만 온다 (완료 기준: done/error)
                runtime::RuntimeEvent::SessionExited { session, exit_code } => {
                    if let Some(raw) = session_titles.get(session).cloned() {
                        let title = ui::workspace::display_pane_title(&raw, catalog);
                        notifications.on_pty_exit(
                            workspace_id,
                            *session,
                            *exit_code,
                            &title,
                            agent_providers.get(session).copied(),
                            catalog,
                        );
                    }
                    session_titles.remove(session);
                }
                _ => {}
            }
        }
    }

    fn persist_agent_session_mutations(&mut self) {
        self.persist_agent_session_mutations_with_policy(false);
    }

    fn persist_agent_session_mutations_with_policy(&mut self, force: bool) {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

        self.agent_persistence_queue
            .extend(self.agent_sessions_ui.drain_persistence_mutations());
        if self.agent_persistence_queue.is_empty() {
            self.agent_persistence_retry_at = None;
            return;
        }
        let now = std::time::Instant::now();
        if !force
            && self
                .agent_persistence_retry_at
                .is_some_and(|retry_at| now < retry_at)
        {
            return;
        }

        if let Err(error) =
            apply_agent_persistence_batch(&self.db, &mut self.agent_persistence_queue)
        {
            let message = format!("구조화 Codex thread 저장 실패: {error:#}");
            tracing::warn!("{message}");
            self.agent_sessions_ui.report_persistence_error(message);
            self.agent_persistence_retry_at = Some(now + RETRY_DELAY);
            return;
        }
        self.agent_persistence_retry_at = None;
    }

    fn record_activity_events(rt: &mut WorkspaceRuntime, events: &[runtime::RuntimeEvent]) {
        for event in events {
            if let runtime::RuntimeEvent::ResourceUsage {
                snapshot,
                session_usage,
            } = event
            {
                rt.resource_usage = Some(*snapshot);
                rt.session_resource_usage = session_usage.clone();
            }
            if let runtime::RuntimeEvent::PtyInputPressure { session, pressure } = event {
                if pressure.queued_messages == 0 && pressure.queued_bytes == 0 {
                    // 해소 신호(워커가 큐 비움 관측, 2026-07-09) — 뱃지 즉시 내림.
                    rt.session_input_pressure.remove(session);
                    rt.input_pressure = rt
                        .session_input_pressure
                        .values()
                        .max_by_key(|(_, at)| *at)
                        .cloned();
                } else {
                    let now = std::time::Instant::now();
                    rt.input_pressure = Some((pressure.clone(), now));
                    rt.session_input_pressure
                        .insert(*session, (pressure.clone(), now));
                }
            }
            // 세션 종료 시 pane별 압력 신호 정리 (stale 뱃지 방지). 워크스페이스 뱃지도
            // 남은 세션들 중 최신으로 재계산 — exit한 세션의 신호가 TTL까지 남지 않게(codex).
            if let runtime::RuntimeEvent::SessionExited { session, .. } = event {
                rt.session_input_pressure.remove(session);
                rt.input_pressure = rt
                    .session_input_pressure
                    .values()
                    .max_by_key(|(_, at)| *at)
                    .cloned();
            }
            // live 세션 추적 (suspend 보호)
            rt.live.observe(event);
            // 이관받은 agent spawn 대기 해소 (성공/실패 어느 쪽이든 응답 도착)
            if matches!(
                event,
                runtime::RuntimeEvent::AgentSpawned { .. }
                    | runtime::RuntimeEvent::SpawnFailed {
                        kind: runtime::SpawnKind::Agent,
                        ..
                    }
            ) {
                rt.pending_agent_spawns = rt.pending_agent_spawns.saturating_sub(1);
            }
        }
    }

    fn poll_pending_approvals(&mut self) {
        match self.db.list_pending_approvals() {
            Ok(rows) => {
                let remote_urls = self.approval_remote_urls(&rows);
                self.approvals_ui.set_pending(rows, remote_urls);
            }
            Err(e) => tracing::warn!("승인 목록 조회 실패: {e:#}"),
        }
    }

    /// pending 승인에 표시할 http 서버 원격 url (server_id → url).
    /// proxy 경유 경로는 Connector Center 신뢰 모달을 거치지 않으므로 첫 Ask 승인이
    /// 원격 전송 고지를 겸한다 (H3 리뷰 P1). pending이 없으면 조회하지 않는다.
    fn approval_remote_urls(
        &self,
        rows: &[storage::PendingApprovalRow],
    ) -> std::collections::HashMap<String, String> {
        if rows.is_empty() {
            return Default::default();
        }
        match self.db.list_mcp_servers() {
            Ok(servers) => servers
                .into_iter()
                .filter(|server| server.kind == "http")
                .filter_map(|server| Some((server.id, server.url?)))
                .collect(),
            Err(e) => {
                tracing::warn!("승인 고지용 MCP 서버 조회 실패: {e:#}");
                Default::default()
            }
        }
    }

    /// 벨 팝오버(대기 인박스 + 최근 알림)의 고정 Id — 단축키(⌘⇧U) 토글이 같은 팝오버를
    /// 가리켜야 하므로 상수 Id를 쓴다.
    fn inbox_popup_id() -> egui::Id {
        egui::Id::new("inbox_popup")
    }

    /// [N3] 전역 PTY 대기 카드 데이터 조립 — 팝오버가 열렸을 때만 호출된다(호출부 게이트,
    /// idle 비용 0). suspended/사라진 워크스페이스는 카드에서 제외한다(I1 "모르는/사라진
    /// 세션이면 명령 미생성" 원칙 — 이동 외엔 아무것도 할 수 없는 죽은 카드를 안 보인다).
    fn build_waiting_cards(&mut self) -> Vec<ui::inbox_waiting::WaitingCard> {
        let global_waiting = self.global_waiting.clone();
        // 활성 워크스페이스 세션의 미리보기(요약)는 session_entries에서 온다 — mux 전체를
        // 순회하는 함수라 카드마다 부르면 팝오버가 열린 동안 매 프레임 × 카드 수만큼
        // 재구성된다. 활성 카드가 하나라도 있을 때 **1회만** 만들어 공유한다.
        let active_summaries: std::collections::HashMap<runtime::SessionId, String> =
            if global_waiting.iter().any(|(ws, _)| *ws == self.active.id) {
                self.active
                    .workspace_ui
                    .session_entries(
                        &self.i18n,
                        &self.agent_activity,
                        &self.agent_needs_input,
                        &self.agent_turn_done,
                    )
                    .into_iter()
                    .filter_map(|entry| Some((entry.session?, entry.summary)))
                    .collect()
            } else {
                std::collections::HashMap::new()
            };
        let mut cards = Vec::with_capacity(global_waiting.len());
        for (ws_id, session) in global_waiting {
            let Some(ws_row) = self.workspaces.iter().find(|w| w.id == ws_id) else {
                continue;
            };
            let workspace_name = Self::workspace_display_name(ws_row);
            let card = if ws_id == self.active.id {
                self.build_active_waiting_card(&ws_id, session, workspace_name, &active_summaries)
            } else if self.warm.contains_key(&ws_id) {
                self.build_warm_waiting_card(&ws_id, session, workspace_name)
            } else {
                None
            };
            if let Some(card) = card {
                cards.push(card);
            }
        }
        cards
    }

    /// [N3] 활성 워크스페이스의 카드 — 미리보기는 tail 재조회 없이 기존 메모리 summary를
    /// 재사용한다(사이드바가 이미 쓰는 것과 같은 "마지막 비어있지 않은 행" 데이터).
    /// `summaries`는 호출측이 1회 만들어 넘긴다(카드마다 재구성 방지).
    fn build_active_waiting_card(
        &self,
        ws_id: &str,
        session: runtime::SessionId,
        workspace_name: String,
        summaries: &std::collections::HashMap<runtime::SessionId, String>,
    ) -> Option<ui::inbox_waiting::WaitingCard> {
        let raw_title = self.active.session_titles.get(&session)?.clone();
        let session_title = ui::workspace::display_pane_title(&raw_title, &self.i18n);
        let preview = summaries
            .get(&session)
            .filter(|summary| !summary.trim().is_empty())
            .map(|summary| vec![summary.clone()]);
        Some(ui::inbox_waiting::WaitingCard {
            workspace_id: ws_id.to_owned(),
            session,
            workspace_name,
            session_title,
            preview,
        })
    }

    /// [N3] warm 워크스페이스의 카드 — 미리보기는 로그 tail(persistent_session_id로 찾은
    /// 파일)에서 읽는다. §14.1: warm은 render 경로가 아니라 mux 스냅샷이 최신이 아닐 수
    /// 있다 — 그 경우 UUID를 못 찾아 미리보기만 비고 카드(버튼)는 정상 동작한다(폴백).
    fn build_warm_waiting_card(
        &mut self,
        ws_id: &str,
        session: runtime::SessionId,
        workspace_name: String,
    ) -> Option<ui::inbox_waiting::WaitingCard> {
        let rt = self.warm.get(ws_id)?;
        let raw_title = rt.session_titles.get(&session)?.clone();
        let uuid = rt
            .workspace_ui
            .mux()
            .and_then(|mux| ui::inbox_waiting::find_persistent_session_id(mux, session));
        // rt(= self.warm의 대여)는 여기서 끝난다 — 아래 미리보기 조회가 self.inbox_waiting_ui를
        // 가변 대여해야 하므로 self.warm을 다시 빌리지 않도록 순서를 나눴다.
        let session_title = ui::workspace::display_pane_title(&raw_title, &self.i18n);
        let preview = uuid.and_then(|uuid| {
            let logs_root = self.logs_base.join(ws_id);
            self.inbox_waiting_ui
                .preview(&self.egui_ctx, &logs_root, &uuid)
        });
        Some(ui::inbox_waiting::WaitingCard {
            workspace_id: ws_id.to_owned(),
            session,
            workspace_name,
            session_title,
            preview,
        })
    }

    /// [N3] 응답 주입 — 워크스페이스 전환 없이 활성/warm 워크스페이스에 직접 WriteInput을
    /// 보낸다. 안전장치: 주입 직전 그 세션이 여전히 대기 중인지 재확인한다(stale 카드가
    /// 엉뚱한 입력을 넣지 않게 — I1 "모르는/사라진 세션이면 명령 미생성" 원칙).
    fn inject_waiting_answer(
        &mut self,
        workspace_id: &str,
        session: runtime::SessionId,
        reply: &str,
    ) {
        let key = format!("{workspace_id}:{}", session.0);
        let still_waiting = self
            .db
            .list_waiting_sessions()
            .unwrap_or_default()
            .iter()
            .any(|k| k == &key);
        if !still_waiting {
            // 대기가 이미 해소됨 — 조용히 무시한다. 다음 refresh_needs_input이 카드
            // 목록을 갱신한다.
            return;
        }
        let bytes = format!("{reply}\n").into_bytes();
        if workspace_id == self.active.id {
            // 선택 중 freeze 해제 — 이 경로도 WorkspaceUi::send를 우회한다
            // (resume 주입 경로와 동일 관례, app.rs의 다른 WriteInput 직접 전송 참고).
            self.active.workspace_ui.clear_selection(session);
            let _ = self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::WriteInput { session, bytes });
        } else if let Some(rt) = self.warm.get_mut(workspace_id) {
            rt.workspace_ui.clear_selection(session);
            let _ = rt
                .runtime
                .send_command(runtime::RuntimeCommand::WriteInput { session, bytes });
        }
        // suspended/사라진 워크스페이스면 runtime이 없어 자연히 no-op(I1) — build_waiting_cards가
        // 애초에 그런 세션의 카드를 만들지 않으므로 정상 경로에서는 도달하지 않는다.
    }

    /// 벨 팝오버 본문 (v3.9 N1). 설정 창과 독립 — 밖을 클릭하면 닫힌다.
    /// 반환: 최근 알림 / 승인 카드 / PTY 대기 카드의 [이동→]에서 클릭한 대상
    /// (있으면 호출측이 기존 네비게이션 경로로 처리).
    ///
    /// 「대기 중」 섹션 = MCP 승인 카드(N2) + PTY 입력 대기 카드(N3) — 둘 다 그 워크스페이스로
    /// 이동하지 않고 처리하는 것이 목적이다(v3.9). 그 아래가 「최근 알림」(지나간 기록).
    fn inbox_popup(
        &mut self,
        bell: &egui::Response,
        text: &i18n::Catalog,
    ) -> Option<ui::notifications::AgentNotificationTarget> {
        const RECENT_IN_POPOVER: usize = 5;
        let mut clicked = None;
        let mut open_full = false;
        // 승인 카드마다 워크스페이스명이 필요하다(핵심 요구 — 가지 않고 판단). 표시
        // 이름은 .show() 진입 전에 소유 데이터로 미리 계산해 둔다 — closure 안에서
        // self.workspaces를 빌리면 아래 self.notifications_ui(&mut) 차용과 얽힌다.
        let approval_workspace_names: std::collections::HashMap<String, String> = self
            .workspaces
            .iter()
            .map(|w| (w.id.clone(), Self::workspace_display_name(w)))
            .collect();
        let mut approval_decision = None;
        let popup_open = egui::Popup::is_id_open(&self.egui_ctx, Self::inbox_popup_id());
        // PTY 입력 대기 카드 — 팝오버가 열려 있을 때만 조립한다(idle 비용 0:
        // 닫혀 있으면 tail 조회·캐시 갱신을 전혀 하지 않는다).
        let waiting_cards = popup_open.then(|| self.build_waiting_cards());
        let mut waiting_action = None;
        egui::Popup::from_response(bell)
            .id(Self::inbox_popup_id())
            .open_memory(bell.clicked().then_some(egui::SetOpenCommand::Toggle))
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .align(egui::RectAlign::BOTTOM_START)
            .show(|ui| {
                ui.set_min_width(260.0);
                ui.set_max_width(300.0);
                // ── 대기 중 섹션 (처리하면 사라지는 액션 큐) ──
                // MCP 승인 카드 — 이미 폴링된 목록만 읽는다(App::poll_pending_approvals가
                // approval-watcher 신호로 채운다) — 새 DB 조회 없음.
                let approval_action = ui::inbox_approvals::render(
                    ui,
                    text,
                    self.approvals_ui.pending(),
                    &approval_workspace_names,
                );
                approval_decision = approval_action.decision;
                clicked = approval_action.goto;
                // PTY 입력 대기 카드 — 카드가 없으면 아무것도 그리지 않는다.
                if let Some(cards) = &waiting_cards {
                    waiting_action = self.inbox_waiting_ui.render(ui, text, cards);
                    if !cards.is_empty() {
                        ui.add_space(6.0);
                    }
                }
                // ── 최근 알림 섹션 (지나간 기록) ──
                if let Some(target) =
                    self.notifications_ui
                        .recent_section(ui, text, RECENT_IN_POPOVER)
                {
                    clicked = Some(target);
                }
                ui.add_space(6.0);
                // 전체 기록·비우기는 설정→알림이 계속 담당한다 (팝오버는 빠른 확인만).
                if ui
                    .button(text.t("inbox.view_all", &[]))
                    .on_hover_text(text.t("inbox.view_all.hint", &[]))
                    .clicked()
                {
                    open_full = true;
                }
            });
        if open_full {
            self.settings_category = ui::settings::Category::Notifications;
            self.settings_open = true;
            self.refresh_workspaces();
            egui::Popup::close_id(&self.egui_ctx, Self::inbox_popup_id());
        }
        // [N3] 카드 액션 처리 — Popup::show 클로저 밖에서 한다(클로저 내부 빌림 단순화).
        // 응답 주입은 바로 실행하고, 이동은 기존 알림 네비게이션 파이프라인에 태운다
        // (clicked와 같은 반환 타입 — AgentNotificationTarget::Pty 재사용).
        let mut goto = None;
        match waiting_action {
            Some(ui::inbox_waiting::WaitingAction::Answer {
                workspace_id,
                session,
                reply,
            }) => self.inject_waiting_answer(&workspace_id, session, &reply),
            Some(ui::inbox_waiting::WaitingAction::Goto(target)) => goto = Some(target),
            None => {}
        }
        // 팝오버를 연 동안은 읽음 처리 — 설정→알림 카테고리와 같은 규약.
        if popup_open && self.notifications_ui.mark_all_read() {
            self.egui_ctx.request_repaint();
        }
        // 승인/거부 결정 되쓰기 — 기존 모달 경로(approvals_ui.show 처리부)와 동일한
        // resolve_approval 호출. first-writer-wins라 모달·인박스 어느 쪽으로 먼저 처리해도
        // 정합. 워크스페이스 전환 없이 여기서 바로 처리되는 것이 이 기능의 존재 이유.
        if let Some(decision) = approval_decision {
            let now = deppy_core::time::unix_secs_i64();
            if let Err(e) =
                self.db
                    .resolve_approval(&decision.id, decision.allowed, decision.remember, now)
            {
                tracing::warn!("승인 해소 실패(인박스): {e:#}");
            }
            self.prune_resolved_approvals();
            // 해소 직후 목록을 갱신해 카드가 바로 사라지게 한다(다음 폴링을 기다리지 않음).
            self.poll_pending_approvals();
            self.egui_ctx.request_repaint();
        }
        clicked.or(goto)
    }

    fn prune_resolved_approvals(&self) {
        let now = deppy_core::time::unix_secs_i64();
        match self
            .db
            .prune_resolved_approvals(now.saturating_sub(Self::RESOLVED_APPROVAL_RETENTION_SECS))
        {
            Ok(n) if n > 0 => tracing::info!("resolved MCP approval {n}건 정리"),
            Ok(_) => {}
            Err(e) => tracing::warn!("resolved MCP approval 정리 실패: {e:#}"),
        }
    }

    /// eframe renderer feature와 무관한 공통 종료 경로. `App::on_exit` 시그니처만
    /// `glow` feature에 따라 달라지므로 실제 정리는 여기 한 번만 유지한다.
    fn shutdown_on_exit(&mut self) {
        // B1: 링버퍼에 모은 frame 이벤트 flush + 요약/gpu 이벤트. shutdown보다 **먼저** —
        // egui 텍스처 상태가 살아 있어야 gpu 이벤트가 실제 값을 낸다.
        if let Some(bench) = self.bench.as_mut() {
            bench.finish();
        }
        // 마지막 App Server 이벤트가 만든 thread metadata를 종료 전에 한 번 더 반영한다.
        // 평상시 실패분도 FIFO queue에 남아 있으므로 retry deadline과 무관하게 flush한다.
        self.agent_sessions_ui.poll();
        self.persist_agent_session_mutations_with_policy(true);
        // App Server는 PTY runtime과 독립된 child process라 여기서 명시적으로 종료·reap한다.
        self.agent_sessions_ui.shutdown();
        // shutdown join 중 도착한 마지막 thread/start/resume 결과도 controller가 drain한다.
        self.persist_agent_session_mutations_with_policy(true);
        self.approval_watcher.stop();
        // 웹서버(모바일 PWA)를 runtime보다 먼저 정지 — 브리지가 쥔 command_sink가
        // worker 채널을 살려둔 채 join을 기다리는 순환을 끊는다 (P5 리뷰 P1 종료 데드락;
        // runtime shutdown 플래그가 근본 방어이고 이 순서는 이중 방어 + 접속 정리).
        if let Some(state) = self.web.take() {
            state.server.shutdown();
        }
        // remote TLS 서버를 먼저 정지 — accept 루프·접속·전용 worker(그 세션들 reap)를 정리한다.
        if let Some(state) = self.remote.take() {
            state.server.shutdown();
        }
        // worker join까지 동기 대기 — 셸 자식 프로세스 정리(reap) 보장.
        self.active.runtime.shutdown();
        // warm 워커들도 종료 (계속 실행 중이던 세션들 reap).
        for (_, rt) in self.warm.drain() {
            let mut runtime = rt.runtime;
            runtime.shutdown();
        }
        // 전환으로 background 정리 중이던 옛 워커들도 끝까지 join한다 — detached
        // 스레드는 프로세스 종료 시 join되지 않아 PTY reap이 중단될 수 있다 (codex 리뷰).
        for (_, handle) in self.pending_shutdowns.drain(..) {
            let _ = handle.join();
        }
    }
}

impl eframe::App for App {
    #[cfg(feature = "render-glow")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown_on_exit();
    }

    #[cfg(not(feature = "render-glow"))]
    fn on_exit(&mut self) {
        self.shutdown_on_exit();
    }

    // §14.1 Active↔Warm: 창이 안 보이면(최소화/완전 가림) worker가 snapshot 생성을
    // 멈추게 한다(세션은 유지). logic()은 창이 안 보여 ui()가 스킵될 때도 호출되므로
    // 여기서 감지해야 전이를 놓치지 않는다 (eframe 0.35). `visible()`은 eframe이 ui()
    // 스킵 판단에 쓰는 바로 그 신호(minimized OR occluded — macOS는 occluded로 갱신되어
    // minimized 미갱신 문제를 피한다). None(미보고)이면 안전하게 Active 유지.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 파일/SQLite/keyring은 worker에서 끝났고, 여기서는 최신 epoch 결과만 짧게 적용한다.
        self.poll_dotenv_sync();
        // 창이 숨겨져도 App Server JSON-RPC 이벤트를 드레인해 structured session 상태를 최신화한다.
        self.agent_sessions_ui.poll();
        self.persist_agent_session_mutations();
        for notice in self.agent_sessions_ui.drain_status_notices() {
            self.notifications_ui.on_structured_status(
                &notice.workspace_id,
                &notice.session_id,
                notice.status,
                &notice.title,
                &self.i18n,
            );
        }
        let structured_alive = self.agent_sessions_ui.session_ids();
        self.notifications_ui
            .retain_structured_sessions(&structured_alive);
        self.poll_restore_timeout();
        // 설정이 닫혀도 stale generation 결과를 계속 버려 worker의 bounded 결과 큐가
        // 평문 secret을 붙잡은 채 막히지 않게 한다.
        self.poll_env_secret_reveals();

        // macOS 네이티브 메뉴 이벤트 (main.rs install_macos_menu)
        #[cfg(target_os = "macos")]
        while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            if event.id() == "settings" {
                self.settings_open = true;
            }
        }

        // 오프스크린 방어: 실행 중 외부 모니터가 분리되면 창이 존재하지 않는 좌표에
        // 남아 "죽은 것처럼" 보인다 (2026-07-05 실증). 창이 어느 모니터에도 속하지
        // 않으면(monitor_size None — macOS는 완전 오프스크린 창의 screen이 nil)
        // 주 화면 안으로 옮긴다. 쿨다운 2s — 이동 반영 전 재발사 방지.
        let offscreen =
            ctx.input(|i| i.viewport().outer_rect.is_some() && i.viewport().monitor_size.is_none());
        if offscreen && self.last_offscreen_fix.elapsed() >= std::time::Duration::from_secs(2) {
            self.last_offscreen_fix = std::time::Instant::now();
            tracing::warn!("창이 화면 밖 — 주 화면으로 이동");
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(80.0, 80.0)));
        }
        // 시작 위치 강제: 항상 주 화면에 뜬다. NativeOptions.centered는 주 화면 크기로
        // 계산한 좌표를 macOS winit이 보조 모니터 로컬 좌표로 적용하는 문제가 있어
        // (2026-07-05 실증: (255,137) 지정 → 왼쪽 모니터 -2303) 창 생성 후 런타임
        // 명령으로 1회 이동한다 — 이 경로는 글로벌 좌표로 동작한다. 이후 사용자가
        // 옮기는 위치는 존중(1회뿐, persist_window=false라 다음 시작도 여기부터).
        if !self.startup_positioned && ctx.input(|i| i.viewport().outer_rect.is_some()) {
            self.startup_positioned = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                120.0, 60.0,
            )));
            // 이동 직후 key window 상태가 흔들려 키 입력이 일시적으로 안 먹는 사례
            // (2026-07-05 사용자 보고) — 창 포커스를 명시 재요청한다.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        let want_active = ctx.input(|i| i.viewport().visible()) != Some(false);
        if want_active != self.active.render_active {
            self.active.render_active = want_active;
            let state = if want_active {
                runtime::WorkspaceRuntimeState::Active
            } else {
                runtime::WorkspaceRuntimeState::Warm
            };
            let _ = self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::SetWorkspaceState(state));
            if want_active {
                // 재개된 Viewport push는 비동기 — 다음 프레임을 예약해 드레인한다.
                // (안 그러면 hidden 중 종료된 pane이 stale/"연결 중…"에 갇힐 수 있다)
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        // warm 워커의 이벤트는 drain해서 그 워커의 pending_events에 '누적'한다 (버리지
        // 않는다 — SessionExited/StatusChanged 같은 일회성 lifecycle 이벤트를 버리면
        // 재활성 시 종료된 pane이 실행 중으로 보인다, codex 리뷰). 재활성 시 fresh가 아닌
        // 이 누적분을 그대로 ui()가 처리해 상태를 재구성한다. 렌더/알림은 활성만.
        let mut runtime_stream_overflowed = false;
        for rt in self.warm.values_mut() {
            let events = rt.events.drain();
            // active와 동일 — 예산 초과 backlog는 wake가 이미 소진돼 직접 예약해야 한다.
            if rt.events.has_backlog() {
                ctx.request_repaint();
            }
            if rt.events.take_overflowed() {
                runtime_stream_overflowed = true;
                rt.event_overflow_pending = true;
            }
            if !events.is_empty() {
                Self::record_activity_events(rt, &events);
                let agent_providers = rt.workspace_ui.agent_providers();
                // warm workspace도 알림은 만든다 (background 완료/승인 통지) — (ws, session)로
                // 식별해 워커 간 SessionId 충돌을 피한다. 렌더용으로는 pending에 누적.
                Self::process_ws_notifications(
                    &mut self.notifications_ui,
                    &rt.id,
                    &events,
                    &mut rt.session_titles,
                    &agent_providers,
                    &self.i18n,
                );
                rt.pending_events.extend(events);
                // MuxUpdated는 매번 전체 스냅샷이라 오래된 건 최신에 완전히 대체된다.
                // chatty한 warm 워커가 pending_events를 무한 누적하지 않도록 최신 하나만
                // 남기고 합친다 (lifecycle은 순서 보존, Viewport는 세션별 최신본 — replay 정확성).
                // 새 이벤트가 들어온 이 분기에서만 호출돼 프레임마다 도는 걸 피한다.
                coalesce_mux_updated(&mut rt.pending_events);
            }
            if rt.event_overflow_pending && rt.events.durable_backlog_exhausted() {
                rt.events = Self::subscribe_runtime_events(&rt.runtime, ctx);
                rt.event_overflow_pending = false;
                // warm→active 전환 자체가 전체 mux/viewport snapshot을 보내므로 지금은
                // worker를 깨우지 않는다.
            }
        }
        self.runtime_stream_warning |= runtime_stream_overflowed;
        self.evict_idle_warm(std::time::Instant::now());

        // 이벤트 drain + 알림 생성은 non-render 경로인 여기서 한다 (§14.1 Warm:
        // ui()가 스킵돼도 승인/완료/실패 알림은 유지). worker의 wake가 숨겨진 UI를
        // 깨워 이 logic()을 돌린다. 렌더용으로는 pending_events에 쌓아 ui()가 소비한다.
        let new_events = self.active.events.drain();
        if !new_events.is_empty() {
            Self::record_activity_events(&mut self.active, &new_events);
            let agent_providers = self.active.workspace_ui.agent_providers();
            Self::process_ws_notifications(
                &mut self.notifications_ui,
                &self.active.id,
                &new_events,
                &mut self.active.session_titles,
                &agent_providers,
                &self.i18n,
            );
            self.active.pending_events.extend(new_events);
            // 창이 숨겨져(render_active=false) ui()가 스킵되면 active의 pending도 warm처럼
            // 무한 누적된다 — 동일하게 coalesce로 유계화한다. 보일 때는 ui()가 매 프레임
            // take()로 소비해 자라지 않으므로 coalesce가 불필요하다.
            if !self.active.render_active {
                coalesce_mux_updated(&mut self.active.pending_events);
            }
            // 여기서 리페인트를 재요청하지 않는다 — 이벤트를 여기까지 실어나른 모든 경로
            // (emit_gated의 Viewport/InputPressure/ResourceUsage slot + enqueue_durable_event)가
            // 이미 subscribe_runtime_events의 wake로 리페인트를 요청했다. 재요청하면 이번
            // 프레임이 그리는 내용을 위해 프레임을 한 장 더 잡고, egui가 거기에 settle 프레임을
            // 하나 더 붙여 갱신 1회당 3프레임이 된다 (agenttui 실측: 페인트의 70%가 헛 프레임).
        }
        // 예외: drain이 durable 예산(256/프레임)을 다 써 backlog를 남겼으면 다음
        // 프레임을 직접 예약한다 — 남은 이벤트의 wake는 이미 coalesce돼 사라졌으므로
        // 예약하지 않으면 lifecycle 이벤트가 무관한 리페인트까지 굶는다 (codex 리뷰 HIGH).
        if self.active.events.has_backlog() {
            ctx.request_repaint();
        }
        if self.active.events.take_overflowed() {
            self.runtime_stream_warning = true;
            self.active.event_overflow_pending = true;
        }
        if self.active.event_overflow_pending && self.active.events.durable_backlog_exhausted() {
            self.active.events = Self::subscribe_runtime_events(&self.active.runtime, ctx);
            self.active.event_overflow_pending = false;
            self.active.event_resync_pending = self.active.render_active;
        }
        if self.active.event_resync_pending
            && self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::SetWorkspaceState(
                    runtime::WorkspaceRuntimeState::Warm,
                ))
                .is_ok()
            && self
                .active
                .runtime
                .send_command(runtime::RuntimeCommand::SetWorkspaceState(
                    runtime::WorkspaceRuntimeState::Active,
                ))
                .is_ok()
        {
            self.active.event_resync_pending = false;
            ctx.request_repaint();
        }

        if self.approval_poll_requested.swap(false, Ordering::AcqRel) {
            self.poll_pending_approvals();
        }

        // 폰 미러 진입(I1b-2) — 웹 스레드가 큐에 넣은 워크스페이스 전환 요청을 처리한다.
        // ui()가 아닌 여기(logic)에서 — 데스크탑 창이 숨겨져도(폰 전용 사용) 전환돼야 한다.
        self.drain_web_switch_requests();
        self.expire_web_notice();

        // 에이전트 감지 워커 입력 갱신 + 결과 드레인 — ui()가 아닌 여기(logic)에서 해야
        // hidden/minimized로 ui()가 스킵돼도 결과 채널이 누적되지 않는다(codex 리뷰).
        self.poll_agent_detect();

        // 폰 대시보드가 볼 워크스페이스 스냅샷(전체 + 해석된 세션 이름) 동기화.
        // ui()가 아닌 logic()에서 — 창이 숨겨져도 폰에는 최신 구성이 보여야 한다.
        // 브리지가 변화 없으면 무시하므로(값 비교) 유휴 프레임 비용은 사실상 0이다.
        self.sync_web_workspaces(std::time::Instant::now());

        // 렌더러 A/B 실측 드라이버 (B1) — env 미설정이면 즉시 반환한다.
        self.bench_step(ctx);
    }

    // egui 0.35부터 update(&Context) 대신 ui(&mut Ui) 시그니처를 쓴다.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame_stats.begin();
        if let Some(bench) = self.bench.as_mut() {
            bench.frame_begin(ui.ctx());
        }
        let text = self.i18n.clone();
        self.handle_configured_shortcut(ui.ctx());
        let mut unread_before = 0;
        // 벨 팝오버의 최근 알림 클릭 — 설정→알림(notif_click)과 같은 네비게이션 경로로
        // 아래에서 함께 처리한다.
        let mut inbox_click = None;
        // 실효 테마(다크 여부)가 바뀌면 터미널 렌더 캐시를 비운다 — stale galley로 글자가
        // 깨진 채 남던 문제(#7). 설정에서의 명시 변경과 System 테마의 OS 레벨 전환(raw
        // input system_theme, config_changed 안 거침, codex 지적)을 모두 여기서 커버한다.
        let theme_dark = ui.ctx().global_style().visuals.dark_mode;
        if theme_dark != self.last_theme_dark {
            self.last_theme_dark = theme_dark;
            self.active.workspace_ui.clear_render_caches();
            for rt in self.warm.values_mut() {
                rt.workspace_ui.clear_render_caches();
            }
            ui.ctx().request_repaint();
        }
        // .env 변경 폴링 fallback (사이드바 OFF 대비 — 2s 스로틀).
        self.poll_dotenv_change();
        // UI 폰트/터미널 모노(가족·굵기) 설정 변경 hot reload — 폰트 재등록 + 렌더 캐시 무효화.
        if font_settings_changed(
            &self.config,
            &self.last_ui_font,
            &self.last_mono_font,
            &self.last_mono_weight,
        ) {
            self.last_ui_font = self.config.ui.ui_font.clone();
            self.last_mono_font = self.config.terminal.mono_font.clone();
            self.last_mono_weight = self.config.terminal.mono_weight.clone();
            crate::fonts::install_cjk_fallback(
                ui.ctx(),
                self.config.ui.ui_font.as_deref(),
                &self.config.terminal.mono_font,
                &self.config.terminal.mono_weight,
            );
            self.active.workspace_ui.clear_render_caches();
            for rt in self.warm.values_mut() {
                rt.workspace_ui.clear_render_caches();
            }
            ui.ctx().request_repaint();
        }
        // UI 배율 변경 hot reload — egui zoom_factor로 UI 전체 확대/축소(터미널은 아래
        // set_ui_scale 역보정으로 크기 유지). 셀 크기가 변하니 렌더 캐시도 무효화.
        if (self.config.ui.ui_scale - self.last_ui_scale).abs() > f32::EPSILON {
            self.last_ui_scale = self.config.ui.ui_scale;
            ui.ctx().set_zoom_factor(self.config.ui.ui_scale);
            self.active.workspace_ui.clear_render_caches();
            for rt in self.warm.values_mut() {
                rt.workspace_ui.clear_render_caches();
            }
            ui.ctx().request_repaint();
        }
        // 타이틀바 통합 바: 패널 기본 inner_margin(8)을 없애 상단 경계에 붙이고 좌측
        // 여백을 제거한다(#67 사용자). 항목은 신호등 높이(28pt 타이틀바, 중심 y≈14)에
        // 맞춰 세로 중앙 정렬.
        let bar_h = 28.0;
        let top_frame =
            egui::Frame::side_top_panel(&ui.ctx().global_style()).inner_margin(egui::Margin::ZERO);
        egui::Panel::top("top_bar")
            .resizable(false)
            .frame(top_frame)
            .show(ui, |ui| {
                // 빈 곳을 잡으면 창을 드래그로 옮긴다. auto-sized Panel의 max_rect는
                // content 측정 전 매우 커질 수 있으므로 실제 titlebar 높이만 hit-test한다.
                let bar_rect = egui::Rect::from_min_size(
                    ui.cursor().min,
                    egui::vec2(ui.available_width(), bar_h),
                );
                let drag = ui.interact(
                    bar_rect,
                    egui::Id::new("titlebar_drag"),
                    egui::Sense::click_and_drag(),
                );
                if drag.drag_started_by(egui::PointerButton::Primary) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), bar_h),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        // 신호등(닫기/최소화/전체화면) 폭만큼 왼쪽 여백 — macOS.
                        #[cfg(target_os = "macos")]
                        ui.add_space(76.0);
                        // 프레임 없는 텍스트 버튼 — 선택(열린 창)이면 accent-soft 둥근 박스로
                        // 강조, hover 시 옅은 배경 (목업 §타이틀바 선택 하이라이트).
                        // 팝오버 앵커가 필요한 곳(벨)은 tbtn_response로 Response를 받는다.
                        let tbtn = |ui: &mut egui::Ui, label: String, selected: bool| -> bool {
                            tbtn_response(ui, label, selected).clicked()
                        };
                        // 구조화된 Codex App Server 세션은 PTY workspace와 별도 창으로 연다.
                        // raw terminal stream을 파싱/재작성하지 않아 ANSI·full-screen 앱이 보존된다.
                        let agent_sessions_selected = self.agent_sessions_ui.is_open();
                        if tbtn(ui, "Agents".to_owned(), agent_sessions_selected) {
                            self.agent_sessions_ui.toggle();
                        }
                        // 벨(대기 인박스 + 최근 알림) — 설정 창과 독립된 경량 팝오버(v3.9 N1).
                        // 알림 확인에 통합 설정 창 전체를 여는 마찰을 없앤다. unread 뱃지는
                        // 설정 라벨에서 여기로 이관했다.
                        let unread = self.notifications_ui.unread();
                        unread_before = unread;
                        // 뱃지 우선순위: 「대기 중」(MCP 승인 + PTY 입력 대기) 건수가 있으면
                        // unread보다 먼저 보인다 — 대기는 즉시 조치가 필요해 정보성 알림보다
                        // 우선한다. 둘 다 이미 폴링된 값이라 여기서 새 조회가 없다
                        // (승인=approval-watcher, PTY=refresh_needs_input).
                        let waiting = self.approvals_ui.pending().len() + self.global_waiting.len();
                        let bell_label = if waiting > 0 {
                            format!("🔔 {waiting}")
                        } else if unread > 0 {
                            format!("🔔 {unread}")
                        } else {
                            "🔔".to_owned()
                        };
                        // [/N2]
                        let bell_open = egui::Popup::is_id_open(ui.ctx(), Self::inbox_popup_id());
                        let bell = tbtn_response(ui, bell_label, bell_open)
                            .on_hover_text(text.t("top.notifications", &[]));
                        inbox_click = self.inbox_popup(&bell, &text);

                        let sel = self.settings_open;
                        if tbtn(ui, text.t("top.settings", &[]), sel) {
                            self.settings_open = !sel;
                            if self.settings_open {
                                self.refresh_workspaces();
                            }
                        }
                        // 우측: 로케일 · (옵트인) 메모리. 메모리 수치(phys_footprint)는
                        // 지표 특성상 오해 소지가 있어 기본 숨김 — 설정 토글로 켠다.
                        // 패널 margin 0이라 오른쪽 끝 여백을 직접 준다.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(10.0);
                            let locale_short = self
                                .config
                                .i18n
                                .locale
                                .split('-')
                                .next()
                                .unwrap_or(&self.config.i18n.locale);
                            let memory = self
                                .config
                                .ui
                                .show_memory_indicator
                                .then_some(self.active.resource_usage)
                                .flatten();
                            let label = match memory {
                                Some(r) => {
                                    format!("{locale_short} · {}MB", r.rss_bytes / (1024 * 1024))
                                }
                                None => locale_short.to_owned(),
                            };
                            let resp = ui.weak(label);
                            if memory.is_some() {
                                resp.on_hover_text(text.t("top.memory_hint", &[]));
                            }
                        });
                    },
                );
                // 툴바-본문 경계선은 egui Panel::top이 자체로 그린다 — 커스텀 hairline을
                // 추가하면 패널 여백 탓에 끝까지 안 닿는 짧은 선이 겹쳤다(#65 사용자).
            });

        // 세션 기본 제목을 "셀 N" 대신 프로젝트명(폴더명 ≈ 깃 레포명, 없으면 "~")으로
        // 표시하도록 활성 workspace 이름을 WorkspaceUi에 넘긴다(사용자 요청).
        let project_name = self
            .workspaces
            .iter()
            .find(|w| w.id == self.active.id)
            .map(Self::workspace_display_name);
        self.active.workspace_ui.set_project_name(project_name);
        // 터미널 폰트 역보정용 UI 배율 전달(zoom_factor로 커진 만큼 font_size를 되돌린다).
        self.active
            .workspace_ui
            .set_ui_scale(self.config.ui.ui_scale);

        // 폴더 트리 사이드바 (FT-1) — CentralPanel보다 먼저 배치해야 한다 (§9-1).
        // OFF(None)면 Panel 자체를 만들지 않는다 (§6 리소스 0).
        let mut terminal_sessions = self.active.workspace_ui.session_entries(
            &text,
            &self.agent_activity,
            &self.agent_needs_input,
            &self.agent_turn_done,
        );
        // 저장된 에이전트가 있고 지금 실행 중이 아닌 pane — 컨텍스트 메뉴 '이어가기' 노출.
        for entry in &mut terminal_sessions {
            entry.resumable =
                entry.agent_line.is_none() && self.restore_agents.contains_key(&entry.pane.0);
        }
        // 완료/입력대기 주목(6px 레일·펄스) 갱신 + 확인 시 완료 소비. Agents 패널도
        // 같은 상태 원천을 사용하므로 사이드바가 꺼져 있어도 계산한다.
        self.update_session_alerts(&mut terminal_sessions);
        let pty_agent_surfaces = self.pty_agent_surfaces(&terminal_sessions);
        if self.file_tree.is_some() {
            let sidebar_action = self
                .file_tree
                .as_mut()
                .and_then(|tree| tree.panel(ui, &terminal_sessions, &text));
            // 워처의 .env* 변경 신호 → 활성 워크스페이스에서 .env가 바뀌거나 사라져도
            // 즉시 재동기화 + 기본 env 재전송 — 시작/전환 시에만 동기화하면 삭제된
            // .env의 secret이 새 셸에 계속 주입된다(codex High).
            let env_changed = self
                .file_tree
                .as_mut()
                .is_some_and(|tree| !tree.take_env_warning_candidates().is_empty());
            if env_changed {
                self.sync_dotenv_env();
            }
            match sidebar_action {
                // "터미널에 경로 삽입" (FT-3): 포커스된 pane의 세션에 WriteInput —
                // 파일 트리의 유일한 runtime 접점 (§6).
                Some(ui::file_tree::SidebarAction::InsertPath(path)) => {
                    let session = self.active.workspace_ui.mux().and_then(|mux| {
                        mux.focused_pane.as_ref().and_then(|focused| {
                            mux.tabs
                                .iter()
                                .flat_map(|tab| &tab.panes)
                                .find(|pane| &pane.id == focused)
                                .and_then(|pane| pane.session_id)
                        })
                    });
                    match session {
                        Some(session) => {
                            let bracketed =
                                self.active.workspace_ui.session_bracketed_paste(session);
                            let shell_kind = self.active.workspace_ui.session_shell_kind(session);
                            let bytes = ui::workspace::path_insert_paste_bytes(
                                &path, shell_kind, bracketed,
                            );
                            // 선택 중 freeze 해제 — 이 경로는 WorkspaceUi::send를 우회한다(codex).
                            self.active.workspace_ui.clear_selection(session);
                            if let Err(e) = self.active.runtime.send_command(
                                runtime::RuntimeCommand::WriteInput { session, bytes },
                            ) {
                                tracing::warn!("경로 삽입 실패: {e:#}");
                            }
                        }
                        None => tracing::info!("경로 삽입: 활성 터미널 세션 없음 — 무시"),
                    }
                }
                Some(ui::file_tree::SidebarAction::CdPath(path)) => {
                    // 포커스된 터미널에서 이 폴더로 cd 실행 (InsertPath와 같은 세션 해석).
                    let session = self.active.workspace_ui.mux().and_then(|mux| {
                        mux.focused_pane.as_ref().and_then(|focused| {
                            mux.tabs
                                .iter()
                                .flat_map(|tab| &tab.panes)
                                .find(|pane| &pane.id == focused)
                                .and_then(|pane| pane.session_id)
                        })
                    });
                    match session {
                        Some(session) => {
                            let bracketed =
                                self.active.workspace_ui.session_bracketed_paste(session);
                            let shell_kind = self.active.workspace_ui.session_shell_kind(session);
                            let bytes = ui::workspace::cd_paste_bytes(&path, shell_kind, bracketed);
                            self.active.workspace_ui.clear_selection(session);
                            if let Err(e) = self.active.runtime.send_command(
                                runtime::RuntimeCommand::WriteInput { session, bytes },
                            ) {
                                tracing::warn!("cd 삽입 실패: {e:#}");
                            }
                        }
                        None => tracing::info!("cd: 활성 터미널 세션 없음 — 무시"),
                    }
                }
                // 세션 목록 클릭 — 해당 tab/pane으로 전환 (workspace 사이드바)
                Some(ui::file_tree::SidebarAction::FocusSession { tab, pane }) => {
                    let is_active_tab = self
                        .active
                        .workspace_ui
                        .mux()
                        .and_then(|m| m.active_tab.clone())
                        == Some(tab.clone());
                    if !is_active_tab
                        && let Err(e) = self
                            .active
                            .runtime
                            .send_command(runtime::RuntimeCommand::SelectTab { tab })
                    {
                        tracing::warn!("탭 전환 실패: {e:#}");
                    }
                    if let Err(e) = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::FocusPane { pane })
                    {
                        tracing::warn!("pane 포커스 실패: {e:#}");
                    }
                }
                // 사이드바 + 버튼 — 새 셸 (탭바 제거 후 대체 진입점)
                Some(ui::file_tree::SidebarAction::NewShell) => {
                    self.active.workspace_ui.spawn_shell(
                        &self.active.runtime,
                        self.config.terminal.scrollback_lines as usize,
                    );
                }
                // 세션 이름 변경 — pane 제목 갱신(mux 반영 + 영속).
                Some(ui::file_tree::SidebarAction::RenameSession { pane, title }) => {
                    if let Err(e) = self
                        .active
                        .runtime
                        .send_command(runtime::RuntimeCommand::RenamePane { pane, title })
                    {
                        tracing::warn!("세션 이름 변경 실패: {e:#}");
                    }
                }
                // 세션 폴더 열기/경로 복사 — cwd는 감지 캐시 우선, 없으면 일회성 lsof.
                Some(ui::file_tree::SidebarAction::OpenSessionFolder { session }) => {
                    match self.session_cwd_lookup(session) {
                        Some(cwd) => platform::open_path(std::path::Path::new(&cwd)),
                        None => tracing::warn!("세션 cwd 미확인 — Finder 열기 생략"),
                    }
                }
                Some(ui::file_tree::SidebarAction::CopySessionPath { session }) => {
                    match self.session_cwd_lookup(session) {
                        Some(cwd) => ui.ctx().copy_text(cwd),
                        None => tracing::warn!("세션 cwd 미확인 — 경로 복사 생략"),
                    }
                }
                // 같은 폴더에서 새 셸 — cwd 미확인이면 일반 새 셸로 폴백.
                Some(ui::file_tree::SidebarAction::NewShellSameFolder { session }) => {
                    let cwd = self.session_cwd_lookup(session);
                    self.active.workspace_ui.spawn_shell_at(
                        &self.active.runtime,
                        self.config.terminal.scrollback_lines as usize,
                        cwd,
                    );
                }
                // 저장된 에이전트 수동 이어가기 — 자동 이어가기 OFF여도 동작한다.
                Some(ui::file_tree::SidebarAction::ResumeAgent {
                    pane,
                    session,
                    title,
                }) => {
                    let mut finder = crate::agent_detect::TranscriptFinder::new();
                    self.send_agent_resume(&pane.0.clone(), &title, session, &mut finder);
                    self.resumed_panes.insert(pane.0);
                }
                Some(ui::file_tree::SidebarAction::ClosePane { pane }) => {
                    self.active
                        .workspace_ui
                        .request_close_pane(&self.active.runtime, pane);
                }
                None => {}
            }
        }

        // ── 관리/모니터 패널 부수효과 (매 프레임 — 통합 설정 창 표시 여부와 무관) ──
        // 렌더는 통합 설정 창 클로저(아래)에서. 여기선 창이 닫혀 있어도 돌아야 하는
        // 부수효과만: agent 실행 응답 추적, OAuth 결과 drain(→credential 추가 시 캐시 무효화).
        let events = std::mem::take(&mut self.active.pending_events);
        self.agents_ui
            .observe_launch_events(&events, ui.ctx(), &text);
        // 커넥터 백그라운드 결과(tools/call·MCP invoke·OAuth)는 창 표시와 무관하게 매 프레임 소화.
        self.connectors_ui.drain_results(&mut self.db);
        self.connectors_ui.drain_invoke(&self.db);
        let credential_added = {
            let oauth_store = AppOAuthCredentialStore {
                secret_store: &self.secret_store,
                redaction: &self.redaction,
            };
            self.connectors_ui.drain_oauth(&mut self.db, &oauth_store)
        };
        if credential_added {
            self.credentials_ui.invalidate_cache();
            // env 뷰의 시크릿 콤보/마스킹도 새 credential을 봐야 한다(PR-ENV-C 배선).
            self.invalidate_env_profile_ui();
            ui.ctx().request_repaint();
        }
        // 작업창은 여백 없이 경계까지 채운다 — CentralPanel 기본 inner_margin(8) 탓에
        // pane 좌/상/우 여백이 보였다(#69 사용자).
        // 작업창 배경은 테마 무관 항상 다크(#18181c) — 라이트 테마에서 터미널 하단
        // 여백/pane 틈에 밝은 패널색이 드러났다(#80·#81).
        let central_frame = egui::Frame::central_panel(&ui.ctx().global_style())
            .inner_margin(egui::Margin::ZERO)
            .fill(egui::Color32::from_rgb(0x18, 0x18, 0x1c));
        egui::CentralPanel::default()
            .frame(central_frame)
            .show(ui, |ui| {
                self.active.workspace_ui.show(
                    ui,
                    &self.config.terminal,
                    &self.active.runtime,
                    &events,
                    &text,
                );
            });
        if self.active.workspace_ui.take_terminal_focus_claimed() {
            self.agent_sessions_ui.surrender_text_focus(ui.ctx());
        }
        // 활성 프로젝트 루트를 Codex thread/start / turn/start의 cwd로 넘긴다. 경로가
        // 비어 있거나 사라졌으면 생략해 App Server의 현재 작업 폴더를 존중한다.
        let agent_workspace_cwd = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == self.active.id)
            .map(|workspace| workspace.path.clone())
            .filter(|path| !path.trim().is_empty())
            .filter(|path| std::path::Path::new(path).is_dir());
        let active_workspace_id = self.active.id.clone();
        let agent_requests = self.agent_sessions_ui.show(
            ui.ctx(),
            &active_workspace_id,
            agent_workspace_cwd,
            pty_agent_surfaces,
        );
        for request in agent_requests {
            self.handle_agent_sessions_request(request);
        }
        // pane 우클릭 → 환경변수·API 설정 (E4 ⑥) — 프로젝트 화면에서 바로 진입.
        if self.active.workspace_ui.take_open_environment() {
            self.settings_category = ui::settings::Category::Environment;
            self.settings_open = true;
            self.refresh_workspaces();
            // T1: 우클릭 진입 시에만 focused 세션 cwd를 감지 — env 페이지 상단에
            // "새 프로젝트로 등록"/"이 폴더를 프로젝트 폴더로 지정" 배너를 띄운다.
            // refresh_workspaces 이후에 감지해 최신 workspace path 목록과 비교한다.
            self.env_session_banner = self.detect_session_cwd_banner();
        }

        // 알림 센터 렌더 (생성은 logic()에서 끝났다). 활성 workspace의 사라진 세션의
        // 진행형 알림 정리 (다른 workspace 건 alive를 알 수 없어 유지).
        let mux = self.active.workspace_ui.mux().cloned();
        if let Some(mux) = &mux {
            let alive: Vec<_> = mux
                .tabs
                .iter()
                .flat_map(|tab| &tab.panes)
                .filter_map(|pane| pane.session_id)
                .collect();
            self.notifications_ui
                .retain_sessions(&self.active.id, &alive);
            // retain_sessions가 배지 그리기 이후 unread를 줄였다면 다음 프레임에 재반영
            if self.notifications_ui.unread() != unread_before {
                ui.ctx().request_repaint();
            }
        }
        // 전환 후 대상 workspace의 mux가 재구성되면(재emit) 알림이 가리킨 세션 pane으로
        // 이동한다 — 전환은 즉시지만 mux는 다음 몇 프레임에 채워지므로 pending으로 둔다.
        if let Some((ws_id, session)) = self.pending_focus.clone() {
            if ws_id != self.active.id {
                self.pending_focus = None; // 다른 곳으로 전환됨 — 취소
            } else if let Some(pane) = mux.as_ref().and_then(|m| pane_of_session(m, session)) {
                let _ = self
                    .active
                    .runtime
                    .send_command(runtime::RuntimeCommand::FocusPane { pane });
                self.pending_focus = None;
            }
        }
        // (알림 클릭 → focus/전환 처리는 통합 설정 창 렌더 이후로 이동 — 클로저에서 캡처)

        // agent-proxy 승인은 벨 팝오버의 「대기 중」 섹션이 처리한다 (v3.9 N4) — 화면 중앙
        // 모달은 **표시하지 않는다**.
        //
        // 모달을 접는 이유: 인박스는 전역(다른 워크스페이스 것 포함) 대기를 한 곳에서
        // 보여주고 그 자리에서 처리하는데, 모달은 큐의 맨 앞 1건만 강제로 띄워 작업을
        // 가로챈다 — 같은 일을 두 곳에서 다르게 하는 셈이다. "무시할 수 없게 알린다"는
        // 모달의 역할은 벨 뱃지 + 기존 OS 알림이 대신한다.
        //
        // `ApprovalsUi`(모달 위젯)와 `show()`는 **의도적으로 남겨 둔다** — 인박스를 써 보고
        // 강제 팝업이 필요하다고 판단되면 이 호출 한 줄을 되살리면 된다. 상태(pending
        // 목록)는 인박스 카드의 소스로 계속 쓰이므로 set_pending 폴링은 그대로다.

        if self.runtime_stream_warning {
            let mut close = false;
            egui::Window::new(text.t("runtime.event_overflow.title", &[]))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(text.t("runtime.event_overflow.body", &[]));
                    ui.add_space(8.0);
                    if ui.button(text.t("action.close", &[])).clicked() {
                        close = true;
                    }
                });
            if close {
                self.runtime_stream_warning = false;
            }
        }

        if let Some(target) = self.warm_limit_warning.clone() {
            let mut close = false;
            egui::Window::new(text.t("workspace.warm_limit.title", &[]))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(text.t(
                        "workspace.warm_limit.body",
                        &[
                            ("target", &target),
                            ("limit", &self.config.performance.max_live_warm.to_string()),
                        ],
                    ));
                    ui.add_space(8.0);
                    if ui.button(text.t("action.close", &[])).clicked() {
                        close = true;
                    }
                });
            if close {
                self.warm_limit_warning = None;
            }
        }

        // 프로젝트 폴더 rename/이동 감지 → 복구 확인 모달 (사용자 요청 2026-07-08).
        if let Some((old, new)) = self.workspace_rename_prompt.clone() {
            let mut decision: Option<bool> = None; // Some(true)=갱신, Some(false)=무시
            egui::Window::new(text.t("workspace.folder_moved.title", &[]))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(text.t("workspace.folder_moved.body", &[]));
                    ui.add_space(4.0);
                    ui.label(text.t("workspace.folder_moved.from", &[("path", &old)]));
                    ui.label(text.t("workspace.folder_moved.to", &[("path", &new)]));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(text.t("workspace.folder_moved.update", &[]))
                            .clicked()
                        {
                            decision = Some(true);
                        }
                        if ui
                            .button(text.t("workspace.folder_moved.ignore", &[]))
                            .clicked()
                        {
                            decision = Some(false);
                        }
                    });
                });
            match decision {
                Some(true) => {
                    // 커밋 직전 재검증(codex Low): 프롬프트가 열린 사이 상태가 변했을 수 있다 —
                    // ①DB 경로가 여전히 old인지 ②new 폴더의 inode가 저장 앵커와 같은지.
                    let db_path = self.db.workspace_path(&self.active.id).ok().flatten();
                    let stored = self.db.workspace_anchor(&self.active.id).ok().flatten();
                    let valid = db_path.as_deref() == Some(old.as_str())
                        && stored.is_some()
                        && Self::folder_anchor(&new) == stored;
                    if valid {
                        // 새 경로로 갱신 + 앵커/env/셸 cwd 재구성 (SetProjectPath와 동일 흐름).
                        if let Err(e) = self.db.set_workspace_path(&self.active.id, &new) {
                            tracing::warn!("폴더 이동 경로 갱신 실패: {e:#}");
                        } else {
                            self.save_workspace_anchor();
                            self.sync_dotenv_env();
                            let cwd = std::path::PathBuf::from(&new);
                            let cwd = cwd.is_dir().then_some(cwd);
                            let _ = self
                                .active
                                .runtime
                                .send_command(runtime::RuntimeCommand::SetShellCwd(cwd.clone()));
                            // 표시명은 path에서 파생(E3) — 별도 갱신 불필요.
                            self.refresh_file_tree_root();
                            self.refresh_workspaces();
                        }
                    } else {
                        tracing::info!("폴더 이동 프롬프트 stale — 갱신 취소");
                    }
                    self.workspace_rename_prompt = None;
                }
                Some(false) => {
                    self.dismissed_renames.insert(self.active.id.clone());
                    self.workspace_rename_prompt = None;
                }
                None => {}
            }
        }

        // known_hosts는 settings 열 때 lazily 로드한다 (닫으면 아래서 None으로 리셋 → 재로드).
        if self.settings_open && self.known_hosts_cache.is_none() {
            let kh = self.load_known_hosts();
            self.known_hosts_cache = Some(kh);
        }
        // env/API 프로젝트 행: Environment 카테고리를 보고 있을 때만 (캐시 만료 시) 재계산.
        // 다른 카테고리 프레임에는 마지막 캐시를 그대로 넘긴다 — category가 show() 안에서
        // 갱신되므로(activity_rows 주석 참조) 탭 전환 프레임에 빈 목록이 번쩍이지 않게.
        // remote_view가 self를 immutable 차용하기 전에 갱신한다(&mut self, borrow 분리).
        let env_api_projects = if self.settings_open
            && self.settings_category == ui::settings::Category::Environment
        {
            self.env_api_project_rows_cached()
        } else if self.settings_open {
            self.env_api_projects_cache
                .as_ref()
                .map(|(rows, _)| rows.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // 설정창 닫힘 전이 — env secret 평문 캐시를 메모리에서 정리(codex Med:
        // 기본 노출로 상주하는 평문의 수명을 설정창 열림 동안으로 한정). remote_view가
        // self 일부를 immutable 차용하기 전에 처리한다.
        if self.settings_was_open && !self.settings_open {
            self.invalidate_env_profile_ui();
        }
        self.settings_was_open = self.settings_open;
        // Remote 뷰모델을 현재 상태에서 구성 (UI는 서버를 직접 만지지 않는다 — disjoint 필드 차용).
        let remote_view = {
            let (running, addr, fp, token) = match &self.remote {
                Some(s) => (
                    true,
                    Some(s.server.local_addr().to_string()),
                    Some(s.fingerprint.as_str()),
                    Some(s.server.auth_token()),
                ),
                None => (false, None, None, None),
            };
            ui::settings::RemoteView {
                running,
                addr,
                fingerprint: fp,
                token,
                error: self.remote_error.as_deref(),
                known_hosts_path: self.known_hosts_path().display().to_string(),
                known_hosts: self.known_hosts_cache.as_deref().unwrap_or(&[]),
            }
        };
        // ts.net 호스트명 자동 감지: 결과 수령 → 설정 반영. MobileWeb 페이지를 처음 열었고
        // 호스트명이 비어 있으면 1회 자동 시도한다 (상주 폴링 없음 — 완료 스레드가 repaint).
        if let Some(rx) = &self.ts_detect_rx {
            match rx.try_recv() {
                Ok(result) => {
                    if let crate::tailscale::Detected::Hostname(host) = &result
                        && (self.ts_detect_overwrite
                            || self.config.web.ts_hostname.trim().is_empty())
                        && self.config.web.ts_hostname.trim() != host
                    {
                        self.config.web.ts_hostname = host.clone();
                        if let Err(e) = self.config.save(&self.config_path) {
                            tracing::warn!("config 저장 실패: {e:#}");
                            self.web_error = Some(format!("설정 저장 실패: {e:#}"));
                        }
                    }
                    self.ts_detected = Some(result);
                    self.ts_detect_rx = None;
                    self.ts_detect_overwrite = false;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // 스레드 생성 실패 등 — 시도 종료로 처리 (CLI 미발견과 동일 안내).
                    self.ts_detected = Some(crate::tailscale::Detected::CliNotFound);
                    self.ts_detect_rx = None;
                    self.ts_detect_overwrite = false;
                }
            }
        } else if self.settings_open
            && self.settings_category == ui::settings::Category::MobileWeb
            && self.config.web.ts_hostname.trim().is_empty()
            && self.ts_detected.is_none()
        {
            self.ts_detect_rx = Some(crate::tailscale::spawn_detect(ui.ctx().clone()));
        }
        // serve 온보딩(O1): 결과 수령 → 상태 반영. 모바일 웹 설정 페이지를 열었고 서버가
        // 켜져 있으면 1회 자동 진단한다(상주 폴링 없음 — 스레드가 완료 시 repaint).
        if let Some(rx) = &self.serve_rx {
            match rx.try_recv() {
                Ok(state) => {
                    self.serve_state = Some(state);
                    self.serve_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.serve_state = Some(crate::tailscale::ServeState::Unknown);
                    self.serve_rx = None;
                }
            }
        } else if self.settings_open
            && self.settings_category == ui::settings::Category::MobileWeb
            && self.serve_state.is_none()
            && let Some(port) = self.web.as_ref().map(|w| w.server.local_addr().port())
        {
            self.serve_rx = Some(crate::tailscale::spawn_serve_check(ui.ctx().clone(), port));
        }
        // 모바일 웹(PWA) 뷰모델 — remote_view와 동일 규칙 (v3.3 P1).
        let web_view = {
            let (running, addr, url) = match &self.web {
                Some(state) => {
                    let addr = state.server.local_addr();
                    let hostname = self.config.web.ts_hostname.trim();
                    let url = web_remote::pairing::access_url(
                        (!hostname.is_empty()).then_some(hostname),
                        addr.port(),
                        &state.token,
                    );
                    (true, Some(addr.to_string()), Some(url))
                }
                None => (false, None, None),
            };
            ui::settings::WebRemoteView {
                running,
                addr,
                url,
                error: self.web_error.as_deref(),
                serve: if self.serve_rx.is_some() {
                    ui::settings::ServeView::Running
                } else {
                    match &self.serve_state {
                        None => ui::settings::ServeView::Idle,
                        Some(crate::tailscale::ServeState::Ready) => ui::settings::ServeView::Ready,
                        Some(crate::tailscale::ServeState::WrongPort(p)) => {
                            ui::settings::ServeView::WrongPort(*p)
                        }
                        Some(crate::tailscale::ServeState::NotConfigured) => {
                            ui::settings::ServeView::NotConfigured
                        }
                        Some(crate::tailscale::ServeState::NotEnabledOnTailnet { approve_url }) => {
                            ui::settings::ServeView::NotEnabled {
                                approve_url: approve_url.as_deref(),
                            }
                        }
                        Some(crate::tailscale::ServeState::Unknown) => {
                            ui::settings::ServeView::Unknown
                        }
                    }
                },
                ts_detect: if self.ts_detect_rx.is_some() {
                    ui::settings::TsDetectView::Running
                } else {
                    match &self.ts_detected {
                        None => ui::settings::TsDetectView::Idle,
                        Some(crate::tailscale::Detected::Hostname(host)) => {
                            ui::settings::TsDetectView::Found(host)
                        }
                        Some(crate::tailscale::Detected::NoHostname) => {
                            ui::settings::TsDetectView::NoHostname
                        }
                        Some(crate::tailscale::Detected::CliNotFound) => {
                            ui::settings::TsDetectView::NoCli
                        }
                    }
                },
            }
        };
        // 알림 카테고리를 보고 있으면 읽음 처리 (기존 notifications.show가 하던 것).
        if self.settings_open
            && self.settings_category == ui::settings::Category::Notifications
            && self.notifications_ui.mark_all_read()
        {
            ui.ctx().request_repaint();
        }
        let notif_unread = self.notifications_ui.unread() as u32;
        // 통합 설정 창: 설정 5개는 settings::show가 인라인, 관리/모니터 7개는 아래
        // render_management 클로저가 각 패널 contents()를 렌더한다 (전체 통합, 2026-07-06).
        // config는 &mut로 넘기므로 클로저는 config 대신 미리 클론한 값을 쓴다 (borrow 분리).
        // 설정창이 열려 있을 때만 조립 — 닫힌 평상시 프레임 비용 0(codex Low). 카테고리까지
        // 조건에 넣으면 탭 전환 프레임에 빈 행이 한 프레임 번쩍이므로(category가 show() 안에서
        // 갱신) 창 열림만 본다.
        let activity_rows = if self.settings_open {
            self.activity_rows()
        } else {
            Vec::new()
        };
        let term_cfg = self.config.terminal.clone();
        let wsid = self.active.id.clone();
        let active_env_api_project = env_api_projects
            .iter()
            .find(|project| project.id == wsid)
            .cloned();
        // 환경변수 편집 게이트(E1 ⑤): 프로젝트 폴더가 지정된 워크스페이스만 .env 편집 허용.
        let env_project_root = self.active_tree_root();
        let env_project_rows_loading =
            self.env_api_projects_cache.is_none() && self.env_project_rows_pending;
        let env_project_rows_failed = self.env_project_rows_failed;
        let db_path = self.db_path.clone();
        let mut activity_action = None;
        let mut notif_click = None;
        let mut ws_switch: Option<String> = None;
        let mut ws_create: Option<std::path::PathBuf> = None;
        let mut ws_delete: Option<String> = None;
        // 프로젝트 삭제 확인 결정 — 모달은 설정 뷰포트 안에서 렌더하고(T2) 결정만 캡처,
        // 실제 삭제/전환은 self 전체 &mut가 필요하므로 클로저 밖에서 처리한다.
        let mut ws_delete_decision: Option<bool> = None;
        let mut workspace_rename: Option<String> = None;
        let mut env_action: Option<ui::env_profiles::EnvAction> = None;
        let mut credentials_changed = false;
        // .env 라이브 반영 토글(E5 ⑨) — 클로저 안에서 편집하고 밖에서 저장/적용.
        let mut env_live_reload_toggle = self.config.ui.env_live_reload;
        // #3 워크스페이스 이름 편집 캡처 (클로저 밖에서 db/refresh 처리 — self 전체 &mut).
        let out = ui::settings::show(
            ui.ctx(),
            &mut self.settings_open,
            &mut self.settings_category,
            &mut self.config,
            &remote_view,
            &mut self.remote_reveal_token,
            &web_view,
            &mut self.web_reveal_url,
            &mut self.web_qr,
            notif_unread,
            &mut self.settings_search,
            &text,
            |ui, cat| {
                use ui::settings::Category as C;
                match cat {
                    // C::Credentials는 settings.rs가 Environment로 리다이렉트 — 분기 불필요
                    // (자격증명 UI는 Environment 뷰의 API 키 섹션으로 통합, 2026-07-09).
                    C::Connectors => {
                        let ctx = ui.ctx().clone();
                        let resolver = AppMcpScopedEnvResolver {
                            secret_store: &self.secret_store,
                            redaction: &self.redaction,
                        };
                        self.connectors_ui.contents(
                            ui,
                            &ctx,
                            &mut self.db,
                            &wsid,
                            &resolver,
                            &text,
                        );
                    }
                    C::Environment => {
                        // 이 화면은 참조 목업처럼 전용 monospace grid를 사용한다.
                        let style = ui.style_mut();
                        style
                            .text_styles
                            .insert(egui::TextStyle::Body, egui::FontId::monospace(14.0));
                        style
                            .text_styles
                            .insert(egui::TextStyle::Button, egui::FontId::monospace(13.0));
                        style
                            .text_styles
                            .insert(egui::TextStyle::Small, egui::FontId::monospace(12.0));
                        // 상세 surface=#242424, 프로젝트 rail은 renderer가 #1e1e1e로 덮는다.
                        ui.painter()
                            .rect_filled(ui.clip_rect(), 0.0, ui.visuals().panel_fill);
                        // T1: 우클릭 진입 시 감지한 세션 폴더 배너 — cwd가 어떤 워크스페이스에도
                        // 속하지 않으면 새 프로젝트 등록, 활성 워크스페이스가 경로 미설정이면
                        // 이 폴더 지정 CTA. 클릭 시 기존 ws_create/SetProjectPath 흐름 재사용.
                        let mut banner_used = false;
                        if let Some(banner) = &self.env_session_banner {
                            let show_register = !banner.registered;
                            let show_set_path = env_project_root.is_none();
                            if show_register || show_set_path {
                                egui::Frame::NONE
                                    .inner_margin(egui::Margin::symmetric(14, 8))
                                    .show(ui, |ui| {
                                        ui.horizontal_wrapped(|ui| {
                                            let display =
                                                ui::env_project_list::display_project_path(
                                                    &banner.cwd.to_string_lossy(),
                                                );
                                            ui.label(text.t(
                                                "env.session_cwd.detected",
                                                &[("path", &display)],
                                            ));
                                            if show_register
                                                && ui
                                                    .button(text.t("env.session_cwd.register", &[]))
                                                    .clicked()
                                            {
                                                ws_create = Some(banner.cwd.clone());
                                                banner_used = true;
                                            }
                                            if show_set_path
                                                && ui
                                                    .button(
                                                        text.t("env.session_cwd.set_project", &[]),
                                                    )
                                                    .clicked()
                                            {
                                                env_action = Some(
                                                    ui::env_profiles::EnvAction::SetProjectPath(
                                                        banner.cwd.clone(),
                                                    ),
                                                );
                                                banner_used = true;
                                            }
                                        });
                                    });
                                ui.separator();
                            }
                        }
                        if banner_used {
                            self.env_session_banner = None;
                        }
                        // 전체 가용 높이를 **먼저** 캡처해 좌측 리스트/우측 스크롤에 강제한다
                        // — horizontal 안에서 available_height가 줄어 리스트가 수십 px로
                        // 잘리던 회귀 방지(2026-07-09 스크린샷).
                        let full_h = ui.available_height();
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.set_min_height(full_h);
                            let project_list_style =
                                ui::env_project_list::EnvProjectListStyle::for_available_width(
                                    ui.available_width(),
                                );
                            match ui::env_project_list::render_with_style(
                                ui,
                                &env_api_projects,
                                &wsid,
                                &text,
                                &project_list_style,
                            ) {
                                ui::env_project_list::EnvProjectListAction::None => {}
                                ui::env_project_list::EnvProjectListAction::Select(id) => {
                                    ws_switch = Some(id);
                                }
                                ui::env_project_list::EnvProjectListAction::AddRequested => {
                                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                        ws_create = Some(dir);
                                    }
                                }
                                ui::env_project_list::EnvProjectListAction::DeleteRequested(id) => {
                                    // 즉시 삭제하지 않고 확인 모달로(5번, 2026-07-10).
                                    let name = env_api_projects
                                        .iter()
                                        .find(|p| p.id == id)
                                        .map(|p| p.name.clone())
                                        .unwrap_or_default();
                                    self.ws_delete_confirm = Some((id, name));
                                }
                            }
                            // 리스트/상세 경계 — separator(6px 스트립)는 우측에 배경
                            // 띠를 남겼다(codex Low) → 1px vline으로 대체.
                            {
                                let h = ui.available_height();
                                let (r, _) = ui
                                    .allocate_exact_size(egui::vec2(1.0, h), egui::Sense::hover());
                                ui.painter().vline(
                                    r.center().x,
                                    r.y_range(),
                                    egui::Stroke::new(
                                        1.0,
                                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                                    ),
                                );
                            }
                            // 우측은 React의 flex column과 동일: 68px 고정 헤더 + body만 scroll.
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.set_min_height(full_h);
                                ui.set_width(ui.available_width());
                                render_env_api_project_header(
                                    ui,
                                    active_env_api_project.as_ref(),
                                    &mut env_action,
                                    &mut workspace_rename,
                                    &mut self.env_api_project_edit,
                                    &text,
                                );
                                egui::ScrollArea::vertical()
                                    .id_salt("env_api_detail_scroll")
                                    .auto_shrink([false, false])
                                    .max_height((full_h - 68.0).max(0.0))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        egui::Frame::NONE
                                            .inner_margin(egui::Margin {
                                                left: 14,
                                                right: 14,
                                                top: 0,
                                                bottom: 12,
                                            })
                                            .show(ui, |ui| {
                                                if env_project_rows_loading {
                                                    ui.horizontal(|ui| {
                                                        ui.add(egui::Spinner::new().size(12.0));
                                                        ui.weak(
                                                            text.t("env.background.loading", &[]),
                                                        );
                                                    });
                                                } else if env_project_rows_failed {
                                                    ui.colored_label(
                                                        ui.visuals().error_fg_color,
                                                        text.t("env.background.load_failed", &[]),
                                                    );
                                                }

                                                // env/API의 ○ reveal은 같은 bounded background
                                                // keyring worker를 공유한다. callback은 캐시 hit을
                                                // 넘기거나 요청을 enqueue할 뿐 UI thread I/O가 없다.
                                                let generation = self.env_secret_generation;
                                                let cache = &mut self.env_secret_cache;
                                                let failures = &self.env_secret_failures;
                                                let pending = &mut self.env_secret_pending;
                                                let worker = &self.env_secret_reveal_worker;
                                                let mut reveal = |credential_id: &str| {
                                                    if let Some(value) = cache.remove(credential_id)
                                                    {
                                                        return Some(value);
                                                    }
                                                    if failures.contains(credential_id) {
                                                        return None;
                                                    }
                                                    if pending.insert(credential_id.to_owned())
                                                        && !worker.try_request(EnvSecretRevealJob {
                                                            generation,
                                                            credential_id: credential_id.to_owned(),
                                                        })
                                                    {
                                                        pending.remove(credential_id);
                                                    }
                                                    None
                                                };
                                                match self.env_profiles_ui.contents_compact(
                                                    ui,
                                                    &mut self.db,
                                                    &wsid,
                                                    env_project_root.as_deref(),
                                                    &mut reveal,
                                                    &text,
                                                ) {
                                                    Ok(a) => {
                                                        if a.is_some() {
                                                            env_action = a;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        ui.colored_label(
                                                            ui.visuals().error_fg_color,
                                                            format!("{e:#}"),
                                                        );
                                                    }
                                                }

                                                let svc = AppCredentialService {
                                                    db: &self.db,
                                                    secret_store: &self.secret_store,
                                                    redaction: &self.redaction,
                                                    workspace_id: &wsid,
                                                };
                                                if self.credentials_ui.contents_compact(
                                                    ui,
                                                    &svc,
                                                    &mut reveal,
                                                    &text,
                                                ) {
                                                    credentials_changed = true;
                                                }

                                                // .env 라이브 반영 토글(E5 ⑨ — 옵트인).
                                                ui.add_space(14.0);
                                                ui.checkbox(
                                                    &mut env_live_reload_toggle,
                                                    text.t("env.live_reload", &[]),
                                                )
                                                .on_hover_text(text.t("env.live_reload_hint", &[]));

                                                if !self.env_secret_pending.is_empty() {
                                                    ui.horizontal(|ui| {
                                                        ui.add(egui::Spinner::new().size(12.0));
                                                        ui.weak(text.t("env.secret.loading", &[]));
                                                    });
                                                }
                                                if !self.env_secret_failures.is_empty() {
                                                    ui.colored_label(
                                                        ui.visuals().error_fg_color,
                                                        text.t("env.secret.load_failed", &[]),
                                                    );
                                                }
                                            });
                                    });
                            });
                        });
                        // 프로젝트 삭제 확인 모달(5번, 2026-07-10) — 설정 뷰포트 안에서
                        // 렌더해 설정 창 위 중앙에 뜨게 한다(T2). ui.ctx()는 현재
                        // immediate 뷰포트(설정 창)라 Window가 그 위에 붙는다. 결정만
                        // 캡처하고 실제 삭제/전환은 클로저 밖에서 처리(self 전체 &mut).
                        if let Some((_, del_name)) = self.ws_delete_confirm.clone() {
                            egui::Window::new(text.t("workspace.delete_confirm.title", &[]))
                                .collapsible(false)
                                .resizable(false)
                                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                                .show(ui.ctx(), |ui| {
                                    ui.label(text.t(
                                        "workspace.delete_confirm.body",
                                        &[("name", &del_name)],
                                    ));
                                    ui.add_space(8.0);
                                    ui.horizontal(|ui| {
                                        if ui
                                            .button(text.t("workspace.delete_confirm.go", &[]))
                                            .clicked()
                                        {
                                            ws_delete_decision = Some(true);
                                        }
                                        if ui.button(text.t("action.cancel", &[])).clicked() {
                                            ws_delete_decision = Some(false);
                                        }
                                    });
                                });
                        }
                    }
                    C::Agents => {
                        self.agents_ui.contents(
                            ui,
                            &self.db,
                            &wsid,
                            &term_cfg,
                            &self.active.runtime,
                            &db_path,
                            &text,
                        );
                    }
                    C::Workspaces => {
                        // 목록 + 전환 + 이름 편집(#3). 전환·저장은 워커 재구성/refresh라
                        // 창 밖에서 처리하도록 캡처만 한다.
                        // 이름 지정(이름 변경) 기능은 제거(2026-07-08 사용자) — 워크스페이스
                        // 이름은 항상 프로젝트 폴더명(경로 미설정이면 "~"). 세부 구분은
                        // 세션(pane) 이름 직접 수정으로 한다.
                        // 새 워크스페이스(B안 2026-07-08): 폴더 선택 → 프로젝트별 격리
                        // 워크스페이스 생성 + 즉시 전환. 처리(생성/전환)는 창 밖에서.
                        if ui
                            .button(text.t("workspace.manager.new", &[]))
                            .on_hover_text(text.t("workspace.manager.new_hint", &[]))
                            .clicked()
                            && let Some(dir) = rfd::FileDialog::new().pick_folder()
                        {
                            ws_create = Some(dir);
                        }
                        ui.add_space(6.0);
                        for ws in &self.workspaces {
                            ui.horizontal(|ui| {
                                let display = Self::workspace_display_name(ws);
                                if ws.id == wsid {
                                    ui.strong(&display);
                                    ui.weak(text.t("workspace.manager.current", &[]));
                                } else {
                                    ui.label(&display);
                                    if ui.button(text.t("workspace.manager.switch", &[])).clicked()
                                    {
                                        ws_switch = Some(ws.id.clone());
                                    }
                                }
                            });
                        }
                    }
                    C::Activity => {
                        activity_action = self.activity_ui.contents(ui, &text, &activity_rows);
                    }
                    C::Notifications => {
                        notif_click = self.notifications_ui.contents(ui, &text);
                    }
                    _ => {}
                }
            },
        );
        // T1: 설정 창이 닫히면 세션 폴더 배너를 버린다 — 다음 우클릭 진입에서 재감지.
        if !self.settings_open {
            self.env_session_banner = None;
        }
        // 관리/모니터 액션 처리 (클로저 밖 — self 전체 &mut 필요한 것들)
        if env_live_reload_toggle != self.config.ui.env_live_reload {
            self.config.ui.env_live_reload = env_live_reload_toggle;
            if let Err(e) = self.config.save(&self.config_path) {
                tracing::warn!("env 라이브 반영 설정 저장 실패: {e:#}");
            }
            // 다음 동기화가 세션 기본 env(활성 조건)를 갱신한다 — 새 셸부터 적용.
            self.sync_dotenv_env();
        }
        if credentials_changed {
            self.invalidate_env_profile_ui();
            self.invalidate_env_api_projects();
        }
        if let Some(name) = workspace_rename {
            // E3: name 컬럼은 별칭 — 빈 값 허용(별칭 해제, 폴더명만 표시).
            let name = name.trim();
            if let Err(e) = self.db.rename_workspace(&self.active.id, name) {
                tracing::warn!("워크스페이스 이름 저장 실패: {e:#}");
            } else {
                self.refresh_workspaces();
            }
        }
        // 환경 메뉴에서 프로젝트 폴더 설정 → workspace path 저장 + .env 재동기화 + 파일트리 루트.
        if let Some(ui::env_profiles::EnvAction::DotenvWrite { key, value }) = &env_action {
            // 7·8번(2026-07-10): UI 편집을 .env 파일에 라인 단위 반영 → 즉시 재동기화.
            if let Some(root) = self.active_tree_root() {
                if let Err(e) = crate::dotenv_sync::write_env_var(&root, key, value.as_deref()) {
                    tracing::warn!(".env 기록 실패: {e:#}");
                } else {
                    self.sync_dotenv_env();
                    self.invalidate_env_profile_ui();
                    self.invalidate_env_api_projects();
                }
            }
        }
        if let Some(ui::env_profiles::EnvAction::Resync) = env_action {
            // 리프레시(4번): .env 계열 재스캔 + UI/카운트 캐시 무효화.
            self.sync_dotenv_env();
            self.invalidate_env_profile_ui();
            self.credentials_ui.invalidate_cache();
            self.invalidate_env_api_projects();
        }
        if let Some(ui::env_profiles::EnvAction::SetProjectPath(path)) = env_action {
            let path_str = path.to_string_lossy().into_owned();
            if let Err(e) = self.db.set_workspace_path(&self.active.id, &path_str) {
                tracing::warn!("프로젝트 폴더 저장 실패: {e:#}");
            } else {
                if path_str.trim().is_empty() {
                    // 해제(2026-07-10): dotenv profile/변수/전용 credential 정리 —
                    // 해제했는데 관련 키가 화면·주입에 남지 않게. .env가 원본이라 안전.
                    if let Err(e) = crate::dotenv_sync::remove_workspace_dotenv(
                        &mut self.db,
                        &self.secret_store,
                        &self.active.id,
                    ) {
                        tracing::warn!("dotenv 정리 실패: {e:#}");
                    }
                    self.invalidate_env_profile_ui();
                    self.credentials_ui.invalidate_cache();
                    self.invalidate_env_api_projects();
                }
                self.save_workspace_anchor(); // rename 복구용 (dev,ino) 앵커
                self.dismissed_renames.remove(&self.active.id);
                self.sync_dotenv_env(); // .env → profile + SetSessionDefaultEnv(새 셸에 적용)
                // active runtime의 셸 cwd도 갱신 — 새 셸/에이전트가 이 폴더에서 뜨게(codex High).
                let new_cwd = std::path::PathBuf::from(&path_str);
                let cwd = (new_cwd.is_dir()).then_some(new_cwd);
                let _ = self
                    .active
                    .runtime
                    .send_command(runtime::RuntimeCommand::SetShellCwd(cwd.clone()));
                // 표시명은 workspace_display_name이 path에서 파생한다(E3) — 별도 갱신 불필요.
                self.refresh_file_tree_root();
                self.refresh_workspaces();
            }
        }
        // 프로젝트 삭제 확인 결정 처리(5번, 2026-07-10) — 목록/DB에서만 제거, 폴더·.env는
        // 보존. 모달 자체는 설정 뷰포트 안에서 렌더하고(T2) 여기서는 캡처한 결정만 처리한다.
        if let Some((del_id, _del_name)) = self.ws_delete_confirm.clone() {
            match ws_delete_decision {
                Some(true) => {
                    if let Err(error) = self.agent_sessions_ui.prepare_workspace_delete(&del_id) {
                        let message = format!("workspace 삭제 중단: {error:#}");
                        tracing::warn!("{message}");
                        self.agent_sessions_ui.report_persistence_error(message);
                        self.ws_delete_confirm = None;
                        // APP thread가 살아 있으면 runtime/credential/DB 어느 것도 건드리지 않는다.
                    } else {
                        // Controller에서 이미 drain된 upsert도 삭제 뒤 workspace를 되살리려
                        // 재시도하면 안 된다. archive/delete는 cascade 뒤 no-op이어도 안전하다.
                        self.agent_persistence_queue.retain(|mutation| {
                            !matches!(
                                mutation,
                                ui::agent_sessions::AgentSessionPersistenceMutation::Upsert {
                                    workspace_id,
                                    ..
                                } if workspace_id == &del_id
                            )
                        });
                        if self.agent_persistence_queue.is_empty() {
                            self.agent_persistence_retry_at = None;
                        }
                        // 활성 프로젝트면 다른 프로젝트로 먼저 전환(삭제 가드가 active를 거부).
                        if del_id == self.active.id
                            && let Some(other) = self
                                .workspaces
                                .iter()
                                .find(|w| w.id != del_id)
                                .map(|w| w.id.clone())
                        {
                            self.switch_workspace(&other);
                            self.refresh_workspaces();
                        }
                        // keyring까지 정리(dotenv 소유 credential) 후 DB 삭제 — 화면·DB에서만
                        // 제거되고 폴더/.env 파일은 보존(재등록 시 복구).
                        if let Err(e) = crate::dotenv_sync::remove_workspace_dotenv(
                            &mut self.db,
                            &self.secret_store,
                            &del_id,
                        ) {
                            tracing::warn!("dotenv 정리 실패: {e:#}");
                        }
                        ws_delete = Some(del_id);
                        self.invalidate_env_api_projects();
                        self.ws_delete_confirm = None;
                    }
                }
                Some(false) => self.ws_delete_confirm = None,
                None => {}
            }
        }

        if let Some(delete_id) = ws_delete {
            if delete_id == self.active.id {
                tracing::info!(
                    "활성 워크스페이스 삭제 요청 무시 — 다른 워크스페이스로 전환 후 삭제 필요"
                );
            } else if self.workspaces.len() <= 1 {
                tracing::info!("마지막 워크스페이스 삭제 요청 무시");
            } else {
                self.join_pending_shutdown(&delete_id);
                if let Some(mut runtime) = self.warm.remove(&delete_id) {
                    runtime.runtime.shutdown();
                }
                self.warm_order.retain(|id| id != &delete_id);
                self.broadcast_terminal_cache_policy();
                self.notifications_ui.prune_workspace(&delete_id);
                match self.db.delete_workspace(&delete_id) {
                    Ok(()) => self.refresh_workspaces(),
                    Err(e) => {
                        tracing::warn!("워크스페이스 삭제 실패: {e:#}");
                        // Controller projection was pruned before the cascade guard. DB가
                        // 남았다면 다시 import해 UI와 durable metadata를 즉시 맞춘다.
                        self.refresh_workspaces();
                    }
                }
            }
        }
        // 새 워크스페이스 생성(B안) — 폴더명으로 만들고 path/앵커 저장 후 즉시 전환.
        // 전환(switch_workspace → make_runtime)이 DB의 path/.env를 읽으므로 저장이 먼저다.
        let mut ws_created = false;
        if let Some(dir) = ws_create {
            let path_str = dir.to_string_lossy().into_owned();
            // 같은 폴더의 워크스페이스가 이미 있으면 새로 만들지 않고 그리로 전환
            // (중복 생성 방지 — path unique 제약이 없다, codex Medium).
            if let Some(existing) = self.workspaces.iter().find(|ws| {
                self.db.workspace_path(&ws.id).ok().flatten().as_deref() == Some(path_str.as_str())
            }) {
                ws_switch = Some(existing.id.clone());
            } else {
                let name = crate::agent_detect::project_display_name(
                    &path_str,
                    self.config.ui.session_name_style,
                )
                .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "workspace".to_owned());
                match self.db.create_workspace(&name) {
                    Ok(new_id) => {
                        if let Err(e) = self.db.set_workspace_path(&new_id, &path_str) {
                            // path 없는 워크스페이스로 전환하면 복원/.env가 엉뚱한 루트
                            // 기준으로 시작한다 — 전환하지 않는다(codex Low).
                            tracing::warn!("새 워크스페이스 경로 저장 실패 — 전환 취소: {e:#}");
                        } else {
                            let anchor = Self::folder_anchor(&path_str);
                            let _ = self.db.set_workspace_anchor(
                                &new_id,
                                anchor.map(|a| a.0),
                                anchor.map(|a| a.1),
                            );
                            ws_switch = Some(new_id);
                            ws_created = true;
                        }
                    }
                    Err(e) => tracing::warn!("워크스페이스 생성 실패: {e:#}"),
                }
            }
        }
        if let Some(id) = ws_switch.filter(|id| *id != self.active.id) {
            let switched_new = ws_created;
            {
                self.switch_workspace(&id);
                self.refresh_workspaces();
            }
            // 새로 만든 워크스페이스면 .env를 즉시 동기화 — 2s 폴링을 기다리지 않고
            // 첫 셸부터 그 폴더의 env를 받게 한다(B안).
            if switched_new {
                self.sync_dotenv_env();
            }
        }
        match activity_action {
            Some(ui::activity::ActivityAction::ClearRenderCaches) => {
                // 렌더 캐시만 — 작업/프로세스/스크롤백 무해(2026-07-08 검토). 다음 프레임 재구축.
                self.active.workspace_ui.clear_render_caches();
                for rt in self.warm.values_mut() {
                    rt.workspace_ui.clear_render_caches();
                }
                self.egui_ctx.request_repaint();
            }
            None => {}
        }
        // 설정→알림과 벨 팝오버는 같은 대상 타입을 돌려준다 — 네비게이션 경로 공유.
        if let Some(target) = notif_click.or(inbox_click) {
            let workspace_ids = self
                .workspaces
                .iter()
                .map(|workspace| workspace.id.clone())
                .collect::<Vec<_>>();
            let navigation =
                plan_agent_notification_navigation(&target, &self.active.id, &workspace_ids);
            if let Some(navigation) = navigation {
                match navigation {
                    AgentNotificationNavigation::FocusCurrentPty { session } => {
                        if let Some(pane) = mux.as_ref().and_then(|m| pane_of_session(m, session)) {
                            let _ = self
                                .active
                                .runtime
                                .send_command(runtime::RuntimeCommand::FocusPane { pane });
                        }
                    }
                    AgentNotificationNavigation::SwitchAndFocusPty {
                        workspace_id,
                        session,
                    } => {
                        self.switch_workspace(&workspace_id);
                        self.refresh_workspaces();
                        self.pending_focus = Some((workspace_id, session));
                    }
                    AgentNotificationNavigation::OpenStructured {
                        switch_workspace,
                        session_id,
                    } => {
                        if let Some(workspace_id) = switch_workspace {
                            self.switch_workspace(&workspace_id);
                            self.refresh_workspaces();
                        }
                        self.agent_sessions_ui.open_session(&session_id);
                    }
                }
                // Settings는 별도 native viewport다. 대상 전환 후 그대로 앞에 남으면
                // 이동이 실패한 것처럼 보이므로 닫고 root workspace를 key window로 올린다.
                self.settings_open = false;
                ui.ctx()
                    .send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Focus);
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
        if out.config_changed {
            self.config.i18n.locale = i18n::normalize_locale(&self.config.i18n.locale);
            if self.i18n.locale() != self.config.i18n.locale {
                self.i18n = load_catalog(&self.config.i18n.locale);
            }
            // hot reload: 테마는 즉시 적용 (터미널 캐시 clear는 ui() 상단의 실효 테마
            // 감지가 다음 프레임에 처리 — System 전환까지 한 경로로 커버).
            ui.ctx().set_theme(self.config.ui.theme.to_egui());
            // 에이전트 상태 hook 토글(agent_status_hooks) 반영 — 설치/해제.
            self.sync_agent_hooks();
            // 터미널 캐시 정책(exited cap/예산) — 활성+warm 워커에 live 반영 (§14.3 확장).
            self.broadcast_terminal_cache_policy();
            // 폴더 트리 hot toggle (§6): OFF → 상태 drop(리소스 0), ON → 즉시 생성
            if self.config.ui.file_tree_enabled != self.file_tree.is_some() {
                self.file_tree = self
                    .config
                    .ui
                    .file_tree_enabled
                    .then(|| self.make_file_tree());
            }
            if let Err(e) = self.config.save(&self.config_path) {
                tracing::warn!("config 저장 실패: {e:#}");
                // remote 포트 등은 접근 표면에 영향 — 저장 실패를 UI에도 남긴다 (codex xhigh Low).
                self.remote_error = Some(format!("설정 저장 실패: {e:#}"));
            }
        }
        match out.remote_action {
            ui::settings::RemoteAction::Start => self.remote_enable(),
            ui::settings::RemoteAction::Stop => self.remote_disable(),
            ui::settings::RemoteAction::Forget(host) => {
                let path = self.known_hosts_path();
                match runtime::known_hosts::KnownHosts::load(&path)
                    .and_then(|mut kh| kh.forget(&host))
                {
                    Ok(()) => {}
                    Err(e) => tracing::warn!("known_hosts forget 실패: {e:#}"),
                }
                self.known_hosts_cache = Some(self.load_known_hosts());
            }
            ui::settings::RemoteAction::None => {}
        }
        match out.web_action {
            ui::settings::WebRemoteAction::Start => self.web_enable(),
            ui::settings::WebRemoteAction::Stop => self.web_disable(),
            ui::settings::WebRemoteAction::RotateToken => self.web_rotate_token(),
            ui::settings::WebRemoteAction::DetectHostname => {
                // 수동 감지 — 기존 값 덮어쓰기 허용. 이미 진행 중이면 무시.
                if self.ts_detect_rx.is_none() {
                    self.ts_detect_overwrite = true;
                    self.ts_detect_rx = Some(crate::tailscale::spawn_detect(ui.ctx().clone()));
                }
            }
            // serve 온보딩 (O1) — CLI 실행은 이 버튼 경로에서만(자동 실행 금지).
            ui::settings::WebRemoteAction::CheckServe => {
                if self.serve_rx.is_none()
                    && let Some(port) = self.web.as_ref().map(|w| w.server.local_addr().port())
                {
                    self.serve_rx =
                        Some(crate::tailscale::spawn_serve_check(ui.ctx().clone(), port));
                }
            }
            ui::settings::WebRemoteAction::ConfigureServe => {
                if self.serve_rx.is_none()
                    && let Some(port) = self.web.as_ref().map(|w| w.server.local_addr().port())
                {
                    self.serve_rx = Some(crate::tailscale::spawn_serve_configure(
                        ui.ctx().clone(),
                        port,
                    ));
                }
            }
            ui::settings::WebRemoteAction::OpenApproveUrl(url) => {
                // tailnet 관리 콘솔 승인 — 앱이 대신할 수 없는 유일한 단계.
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
            ui::settings::WebRemoteAction::None => {}
        }
        // settings가 닫혔으면 표시 상태를 리셋 — 다음에 열 때 known_hosts를 fresh 로드하고
        // 토큰은 다시 마스킹한다. QR 텍스처도 반환한다(다시 열면 재생성).
        if !self.settings_open {
            self.known_hosts_cache = None;
            self.remote_reveal_token = false;
            self.web_reveal_url = false;
            self.web_qr = None;
        }
        self.frame_stats.end();
        // B1: 이번 프레임에 그린 터미널 렌더 카운터를 프레임 이벤트에 실어 보낸다.
        // frame_stats.end() 뒤라 JSONL 기록 비용은 ui_ms에 섞이지 않는다.
        // 스냅샷 관측도 여기서 — logic()에서 보면 다음 프레임까지 밀려 first_snapshot이
        // first_render보다 늦게 찍힌다(첫 실측에서 발견).
        if self.bench.is_some() {
            let counters = self.active.workspace_ui.frame_counters();
            let has_snapshot = self.active.workspace_ui.any_snapshot();
            if let Some(bench) = self.bench.as_mut() {
                if has_snapshot {
                    bench.note_first_snapshot();
                }
                bench.frame_end(counters);
            }
        }
    }
}

/// Instant → 경과 ms (벤치 ws_step용).
fn elapsed_ms(started: std::time::Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// known_hosts 파일 텍스트를 (host, 지문) 목록으로 파싱한다 (settings 표시 전용 —
/// forget/pin은 runtime::known_hosts API로 처리). 포맷은 한 줄에 `host 지문`, `#` 주석·빈
/// 줄은 스킵 (known_hosts 파일 계약과 동일). 파일 순서를 보존한다.
fn parse_known_hosts(text: &str) -> Vec<(String, String)> {
    // 표시도 KnownHosts::load와 같은 **effective view**를 쓴다 — host 중복은 last-wins,
    // 지문은 소문자 정규화. 수동 편집으로 duplicate가 생겨도 실제 신뢰 판단과 다른 낡은
    // 지문을 "신뢰 기록"처럼 보여주지 않는다 (codex xhigh Low).
    let mut rows: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        if let (Some(host), Some(fp)) = (parts.next(), parts.next()) {
            let fp = fp.to_ascii_lowercase();
            match rows.iter_mut().find(|(h, _)| h == host) {
                Some(row) => row.1 = fp, // last-wins (KnownHosts HashMap과 동일)
                None => rows.push((host.to_owned(), fp)),
            }
        }
    }
    rows
}

/// 세션이 붙어 있는 pane id를 mux 스냅샷에서 찾는다 (알림 클릭 → focus용).
/// 상단바 텍스트 버튼 — 프레임 없이 라벨만, 선택 시 accent-soft 박스.
/// Response를 돌려주므로 팝오버 앵커/hover 텍스트에 쓸 수 있다.
fn tbtn_response(ui: &mut egui::Ui, label: String, selected: bool) -> egui::Response {
    let accent = ui.visuals().selection.bg_fill;
    let col = if selected {
        accent
    } else {
        ui.visuals().weak_text_color()
    };
    let font = egui::FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(label, font, col);
    let w = galley.size().x + 20.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::click());
    if selected {
        ui.painter()
            .rect_filled(rect, 6.0, accent.gamma_multiply(0.15));
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 6.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let pos = egui::pos2(
        rect.center().x - galley.size().x / 2.0,
        rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(pos, galley, col);
    resp
}

fn pane_of_session(
    mux: &runtime::MuxSnapshot,
    session: runtime::SessionId,
) -> Option<runtime::MuxPaneId> {
    mux.tabs
        .iter()
        .flat_map(|tab| &tab.panes)
        .find(|pane| pane.session_id == Some(session))
        .map(|pane| pane.id.clone())
}

fn tab_of_agent_target(
    mux: &runtime::MuxSnapshot,
    pane_id: &runtime::MuxPaneId,
    session_id: runtime::SessionId,
) -> Option<runtime::MuxTabId> {
    mux.tabs
        .iter()
        .find(|tab| {
            tab.panes
                .iter()
                .any(|pane| &pane.id == pane_id && pane.session_id == Some(session_id))
        })
        .map(|tab| tab.id.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentNotificationNavigation {
    FocusCurrentPty {
        session: runtime::SessionId,
    },
    SwitchAndFocusPty {
        workspace_id: String,
        session: runtime::SessionId,
    },
    OpenStructured {
        switch_workspace: Option<String>,
        session_id: String,
    },
}

/// Resolve notification navigation before mutating runtimes. Deleted/stale
/// workspace targets fail closed; a cross-workspace PTY target always keeps a
/// deferred exact-session focus, including when the runtime must be rebuilt.
fn plan_agent_notification_navigation(
    target: &ui::notifications::AgentNotificationTarget,
    active_workspace_id: &str,
    known_workspace_ids: &[String],
) -> Option<AgentNotificationNavigation> {
    match target {
        ui::notifications::AgentNotificationTarget::Pty {
            workspace_id,
            session,
        } if workspace_id == active_workspace_id => {
            Some(AgentNotificationNavigation::FocusCurrentPty { session: *session })
        }
        ui::notifications::AgentNotificationTarget::Pty {
            workspace_id,
            session,
        } if known_workspace_ids.contains(workspace_id) => {
            Some(AgentNotificationNavigation::SwitchAndFocusPty {
                workspace_id: workspace_id.clone(),
                session: *session,
            })
        }
        ui::notifications::AgentNotificationTarget::Structured {
            workspace_id,
            session_id,
        } if workspace_id == active_workspace_id => {
            Some(AgentNotificationNavigation::OpenStructured {
                switch_workspace: None,
                session_id: session_id.clone(),
            })
        }
        ui::notifications::AgentNotificationTarget::Structured {
            workspace_id,
            session_id,
        } if known_workspace_ids.contains(workspace_id) => {
            Some(AgentNotificationNavigation::OpenStructured {
                switch_workspace: Some(workspace_id.clone()),
                session_id: session_id.clone(),
            })
        }
        _ => None,
    }
}

fn load_catalog(locale: &str) -> i18n::Catalog {
    i18n::Catalog::load(locale).unwrap_or_else(|e| {
        tracing::warn!("locale catalog 로드 실패({locale}): {e:#}");
        i18n::Catalog::load(i18n::FALLBACK_LOCALE).expect("fallback locale catalog must load")
    })
}

/// warm 풀 상한 초과분 중 축출 가능한(live 세션 없는) workspace를 앞(가장 오래됨)에서부터
/// 고른다. live workspace는 건너뛰며, 그만큼 상한 초과가 허용된다 (작업 보호 우선).
fn warm_eviction_candidates(
    warm_order: &[String],
    max_warm: usize,
    has_live: impl Fn(&str) -> bool,
) -> Vec<String> {
    let overflow = warm_order.len().saturating_sub(max_warm);
    warm_order
        .iter()
        .filter(|id| !has_live(id))
        .take(overflow)
        .cloned()
        .collect()
}

/// 설정의 전역 MB 예산을 resident runtime 수로 나눈 워커별 share.
/// 설정 최소값(32MB)과 resident 최대값(active 1 + warm 12)에서는 1MB 아래로 내려가지
/// 않지만, 잘못된 호출에도 0바이트 정책이 생기지 않도록 1MiB를 최종 하한으로 둔다.
fn per_runtime_cache_budget_bytes(global_budget_mb: u32, resident_runtimes: usize) -> usize {
    const MIB: usize = 1024 * 1024;
    let total = global_budget_mb as usize * MIB;
    (total / resident_runtimes.max(1)).max(MIB)
}

/// live shell 세션 모두가 단일 저CPU 셸 리더만 보유하는지 확인한다.
/// 직접 셸에서 실행한 Codex/Claude는 UI 감지 또는 같은 process group의 자식 수로 보호한다.
fn shell_sessions_are_idle(
    sessions: &[runtime::SessionId],
    usage: &[runtime::SessionResourceUsage],
    is_detected_agent: impl Fn(runtime::SessionId) -> bool,
) -> bool {
    !sessions.is_empty()
        && sessions.iter().all(|session| {
            if is_detected_agent(*session) {
                return false;
            }
            usage
                .iter()
                .find(|sample| sample.session == *session)
                .is_some_and(|sample| {
                    sample.pid.is_some()
                        && sample.process_count == 1
                        && sample.cpu_percent.is_some_and(|cpu| cpu <= 1.0)
                        && !sample.high_cpu
                        && !sample.high_rss
                })
        })
}

fn expired_warm_workspace_ids(
    warm_order: &[String],
    backgrounded_at: impl Fn(&str) -> Option<std::time::Instant>,
    now: std::time::Instant,
    timeout: std::time::Duration,
) -> Vec<String> {
    warm_order
        .iter()
        .filter(|id| {
            backgrounded_at(id).is_some_and(|at| now.saturating_duration_since(at) >= timeout)
        })
        .cloned()
        .collect()
}

/// warm workspace의 pending_events를 합쳐(coalesce) 재활성 replay를 정확+유계로 만든다.
///
/// replay 규칙(중요): pending_events는 재활성 시 workspace_ui.show()로 렌더 상태를
/// 재구성한다. workspace_ui는 SessionExited/SessionStatusChanged/SessionStatusViewChanged를
/// "현재 mux에 그 세션이 있을 때만" 적용하고(session_alive 체크), MuxUpdated는 pane
/// 구조(session_id/title)만 담아 status/exit은 lifecycle 이벤트로만 반영된다.
///
/// 그래서:
/// 1) 최신 MuxUpdated 하나만 남기고 **맨 앞으로 옮긴다**(나머지 MuxUpdated 제거). replay가
///    최신 mux로 현재 세션/pane을 먼저 확립한 뒤 lifecycle 이벤트가 자기 세션을 찾아
///    적용된다 — 최신 mux 뒤에 남은 SessionExited가 session_alive를 통과해 종료 pane이
///    running으로 남는 버그를 막는다. (mux에 없는 detach된 세션의 잔여 이벤트는 무시돼도
///    화면에 안 나오니 무해.)
/// 2) SessionStatusChanged/SessionStatusViewChanged는 세션별 최신 1개만 유지한다(status는
///    last-wins). detector의 Running↔Waiting churn으로 무계 누적되던 것을 O(세션수)로
///    유계화. 유지분 상대 순서는 보존.
/// 3) ResourceUsage는 프로세스/세션 리소스의 현재 상태라 최신 1개만 유지한다.
/// 4) PtyInputPressure는 세션별 현재 입력 큐 상태라 세션별 최신 1개만 유지한다.
/// 5) Viewport는 화면 전체 최신 스냅샷이므로 세션별 최신 1개만 유지한다. warm/숨김 상태의
///    고출력 세션이 pending replay Vec를 출력량만큼 키우지 않게 한다.
/// 6) SessionExited/ShellSpawned/AgentSpawned/SpawnFailed는 전량 순서 보존.
///
/// 알림은 coalesce 전에 process_ws_notifications가 전량 소비하므로(렌더 replay 전용)
/// 공격적으로 줄여도 알림엔 영향이 없다.
fn coalesce_mux_updated(events: &mut Vec<runtime::RuntimeEvent>) {
    // 남길 최신 MuxUpdated(있으면) — 뽑아서 나중에 맨 앞에 재삽입.
    let latest_mux = events
        .iter()
        .rposition(|e| matches!(e, runtime::RuntimeEvent::MuxUpdated { .. }))
        .map(|i| events[i].clone());
    let latest_resource_idx = events
        .iter()
        .rposition(|e| matches!(e, runtime::RuntimeEvent::ResourceUsage { .. }));

    // 세션별 마지막 StatusChanged의 원 인덱스 (나중 것이 이김 → 그 인덱스만 유지).
    let mut latest_status_idx: std::collections::HashMap<runtime::SessionId, usize> =
        std::collections::HashMap::new();
    let mut latest_status_view_idx: std::collections::HashMap<runtime::SessionId, usize> =
        std::collections::HashMap::new();
    let mut latest_input_pressure_idx: std::collections::HashMap<runtime::SessionId, usize> =
        std::collections::HashMap::new();
    let mut latest_viewport_idx: std::collections::HashMap<runtime::SessionId, usize> =
        std::collections::HashMap::new();
    for (i, e) in events.iter().enumerate() {
        if let runtime::RuntimeEvent::SessionStatusChanged { session, .. } = e {
            latest_status_idx.insert(*session, i);
        }
        if let runtime::RuntimeEvent::SessionStatusViewChanged { session, .. } = e {
            latest_status_view_idx.insert(*session, i);
        }
        if let runtime::RuntimeEvent::PtyInputPressure { session, .. } = e {
            latest_input_pressure_idx.insert(*session, i);
        }
        if let runtime::RuntimeEvent::Viewport { session, .. } = e {
            latest_viewport_idx.insert(*session, i);
        }
    }

    let mut idx = 0;
    events.retain(|e| {
        // retain은 원소를 원래 순서대로 한 번씩 방문 → idx로 원 위치를 추적한다.
        let keep = match e {
            // 모든 MuxUpdated 제거 (최신 하나는 아래서 맨 앞에 재삽입).
            runtime::RuntimeEvent::MuxUpdated { .. } => false,
            // 세션별 마지막 StatusChanged만 유지.
            runtime::RuntimeEvent::SessionStatusChanged { session, .. } => {
                latest_status_idx.get(session) == Some(&idx)
            }
            runtime::RuntimeEvent::SessionStatusViewChanged { session, .. } => {
                latest_status_view_idx.get(session) == Some(&idx)
            }
            runtime::RuntimeEvent::ResourceUsage { .. } => latest_resource_idx == Some(idx),
            runtime::RuntimeEvent::PtyInputPressure { session, .. } => {
                latest_input_pressure_idx.get(session) == Some(&idx)
            }
            runtime::RuntimeEvent::Viewport { session, .. } => {
                latest_viewport_idx.get(session) == Some(&idx)
            }
            _ => true,
        };
        idx += 1;
        keep
    });

    if let Some(mux) = latest_mux {
        events.insert(0, mux);
    }
}

/// T1: pane 우클릭 → 환경설정 진입 시 감지한 focused 세션 폴더 배너 상태.
struct EnvSessionCwdBanner {
    /// focused 세션의 현재 작업 폴더 (agent_detect 워커 lsof 소스 재사용).
    cwd: std::path::PathBuf,
    /// cwd가 기존 워크스페이스 path와 일치하거나 그 하위 폴더인가.
    registered: bool,
}

/// T1: cwd가 워크스페이스 path 중 하나와 일치하거나 그 하위 폴더인지 판정
/// (순수 — 테스트 대상). `Path::starts_with`는 컴포넌트 단위라 `/a/bc`가
/// `/a/b`에 속하는 것으로 오판하지 않는다.
fn cwd_belongs_to_any(cwd: &std::path::Path, roots: &[std::path::PathBuf]) -> bool {
    roots.iter().any(|root| cwd.starts_with(root))
}

/// 영속 pane snapshot에서 이 제목의 세션 cwd를 찾는다 (순수 — 테스트 대상).
///
/// **키는 DB에 저장된 raw 제목**("workspace.spawn.shell 3")이다. 호출측이 i18n 렌더된
/// 값("셸 3")을 넘기면 항상 miss가 되어 프로젝트명 해석이 조용히 실패한다 —
/// warm 워크스페이스에서 실제로 그랬다(리뷰 P2-1). session_titles는 raw를 보관한다.
fn pane_cwd<'a>(panes: &'a [(String, String)], raw_title: &str) -> Option<&'a str> {
    panes
        .iter()
        .find(|(title, _)| title == raw_title)
        .map(|(_, cwd)| cwd.as_str())
        .filter(|cwd| !cwd.is_empty())
}

/// 비활성(warm/유휴) 워크스페이스 pane의 표시명 (순수 — 테스트 대상).
/// 활성 워크스페이스의 `resolve_session_title`과 같은 규칙: 사용자가 rename했으면
/// 그대로, 기본 제목("셸 N")이면 세션 cwd의 프로젝트명으로 대체, cwd가 없거나 판별
/// 불가면 기본 제목을 i18n 렌더한 값으로 폴백.
fn activity_session_name(
    raw_title: &str,
    cwd: Option<&str>,
    catalog: &i18n::Catalog,
    resolve_project: impl Fn(&str) -> Option<String>,
) -> String {
    if !ui::workspace::is_default_session_title(raw_title) {
        return ui::workspace::display_pane_title(raw_title, catalog);
    }
    cwd.filter(|cwd| !cwd.is_empty())
        .and_then(resolve_project)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| ui::workspace::display_pane_title(raw_title, catalog))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    #[test]
    fn agent_notification_navigation_preserves_transport_workspace_and_session() {
        use ui::notifications::AgentNotificationTarget as Target;

        let known = vec!["ws-a".to_owned(), "ws-b".to_owned()];
        let current_pty = Target::Pty {
            workspace_id: "ws-a".to_owned(),
            session: runtime::SessionId(7),
        };
        assert_eq!(
            plan_agent_notification_navigation(&current_pty, "ws-a", &known),
            Some(AgentNotificationNavigation::FocusCurrentPty {
                session: runtime::SessionId(7),
            })
        );

        let cross_pty = Target::Pty {
            workspace_id: "ws-b".to_owned(),
            session: runtime::SessionId(8),
        };
        assert_eq!(
            plan_agent_notification_navigation(&cross_pty, "ws-a", &known),
            Some(AgentNotificationNavigation::SwitchAndFocusPty {
                workspace_id: "ws-b".to_owned(),
                session: runtime::SessionId(8),
            })
        );

        let current_app = Target::Structured {
            workspace_id: "ws-a".to_owned(),
            session_id: "app-1".to_owned(),
        };
        assert_eq!(
            plan_agent_notification_navigation(&current_app, "ws-a", &known),
            Some(AgentNotificationNavigation::OpenStructured {
                switch_workspace: None,
                session_id: "app-1".to_owned(),
            })
        );

        let cross_app = Target::Structured {
            workspace_id: "ws-b".to_owned(),
            session_id: "app-2".to_owned(),
        };
        assert_eq!(
            plan_agent_notification_navigation(&cross_app, "ws-a", &known),
            Some(AgentNotificationNavigation::OpenStructured {
                switch_workspace: Some("ws-b".to_owned()),
                session_id: "app-2".to_owned(),
            })
        );

        let stale = Target::Structured {
            workspace_id: "deleted".to_owned(),
            session_id: "app-stale".to_owned(),
        };
        assert_eq!(
            plan_agent_notification_navigation(&stale, "ws-a", &known),
            None
        );
    }

    #[test]
    fn agent_pty_focus_requires_matching_pane_and_session_pair() {
        let pane_id = runtime::MuxPaneId("pane-1".to_owned());
        let tab_id = runtime::MuxTabId("tab-1".to_owned());
        let session_id = runtime::SessionId(9);
        let mux = runtime::MuxSnapshot {
            tabs: vec![runtime::TabSnapshot {
                id: tab_id.clone(),
                title: "agents".to_owned(),
                layout: runtime::LayoutNode::Pane(pane_id.clone()),
                panes: vec![runtime::PaneSnapshot {
                    id: pane_id.clone(),
                    session_id: Some(session_id),
                    title: "Codex".to_owned(),
                    persistent_session_id: None,
                }],
            }],
            active_tab: Some(tab_id.clone()),
            focused_pane: Some(pane_id.clone()),
        };

        assert_eq!(
            tab_of_agent_target(&mux, &pane_id, session_id),
            Some(tab_id)
        );
        assert_eq!(
            tab_of_agent_target(&mux, &pane_id, runtime::SessionId(10)),
            None
        );
        assert_eq!(
            tab_of_agent_target(
                &mux,
                &runtime::MuxPaneId("stale-pane".to_owned()),
                session_id
            ),
            None
        );
    }

    #[test]
    fn structured_persistence_batch_preserves_fifo_from_first_db_failure() {
        use ui::agent_sessions::AgentSessionPersistenceMutation as Mutation;

        let dir = std::env::temp_dir().join(format!(
            "deppy-agent-persistence-test-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("metadata.sqlite3")).unwrap();
        let mut failed = vec![
            Mutation::Upsert {
                local_session_id: "local-bad".to_owned(),
                workspace_id: "missing-workspace".to_owned(),
                thread_id: "thread-bad".to_owned(),
                title: "bad".to_owned(),
                cwd: "/repo".to_owned(),
                model: None,
                favorite: false,
                archived: false,
            },
            Mutation::Delete {
                local_session_id: "must-not-overtake".to_owned(),
            },
        ];
        assert!(apply_agent_persistence_batch(&db, &mut failed).is_err());
        assert!(matches!(
            failed.as_slice(),
            [Mutation::Upsert { local_session_id, .. }, Mutation::Delete { .. }]
                if local_session_id == "local-bad"
        ));

        let workspace_id = db.create_workspace("retry-ok").unwrap();
        let mut successful = vec![
            Mutation::Upsert {
                local_session_id: "local-1".to_owned(),
                workspace_id: workspace_id.clone(),
                thread_id: "thread-1".to_owned(),
                title: "saved".to_owned(),
                cwd: "/repo".to_owned(),
                model: Some("gpt-test".to_owned()),
                favorite: false,
                archived: false,
            },
            Mutation::SetArchived {
                local_session_id: "local-1".to_owned(),
                archived: true,
            },
        ];
        apply_agent_persistence_batch(&db, &mut successful).unwrap();
        assert!(successful.is_empty());
        let rows = db.list_structured_threads(&workspace_id, true).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].archived);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn 시작_font_snapshot은_첫_frame_재설치를_유발하지_않는다() {
        let config = Config::default();
        let last_ui_font = config.ui.ui_font.clone();
        let last_mono_font = config.terminal.mono_font.clone();
        let last_mono_weight = config.terminal.mono_weight.clone();
        assert!(!font_settings_changed(
            &config,
            &last_ui_font,
            &last_mono_font,
            &last_mono_weight,
        ));

        let mut changed = config;
        changed.terminal.mono_weight = "Bold".to_owned();
        assert!(font_settings_changed(
            &changed,
            &last_ui_font,
            &last_mono_font,
            &last_mono_weight,
        ));
    }

    /// ⑦ 검증: .env 외부 수정 감지의 근거인 baseline 상태가 파일 변경/생성/삭제를
    /// 구분한다 — 2초 점검이 이 값의 변화로 재동기화를 트리거한다(B 경로).
    #[test]
    fn dotenv_baseline은_외부_수정과_생성_삭제를_감지한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-state-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // 파일 없음 → (false, None). 경로 없음도 동일.
        assert_eq!(dotenv_state_for_root(Some(&dir)), (false, None));
        assert_eq!(dotenv_state_for_root(None), (false, None));
        // 생성 감지
        std::fs::write(dir.join(".env"), "A=1\n").unwrap();
        let created = dotenv_state_for_root(Some(&dir));
        assert!(created.0 && created.1.is_some());
        // 내용 수정(mtime 변화) 감지 — 에디터/터미널로 직접 고친 경우
        std::thread::sleep(std::time::Duration::from_millis(15));
        std::fs::write(dir.join(".env"), "A=2\n").unwrap();
        let edited = dotenv_state_for_root(Some(&dir));
        assert_ne!(created, edited, "외부 수정이 baseline에 반영되지 않음");
        // .env.local 추가도 감지(병합 대상 전체를 본다)
        std::fs::write(dir.join(".env.local"), "B=1\n").unwrap();
        assert_ne!(edited, dotenv_state_for_root(Some(&dir)));
        // 삭제 감지
        std::fs::remove_file(dir.join(".env")).unwrap();
        std::fs::remove_file(dir.join(".env.local")).unwrap();
        assert_eq!(dotenv_state_for_root(Some(&dir)), (false, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// E3: 표시 이름은 폴더명 파생 + 별칭 병기. 기존 이름은 별칭으로 강등(데이터 무변경).
    #[test]
    fn 워크스페이스_표시명은_폴더명_파생에_별칭을_병기한다() {
        let row = |name: &str, path: &str| crate::storage::WorkspaceRow {
            id: "w".into(),
            name: name.into(),
            path: path.into(),
            created_at: String::new(),
        };
        // 폴더명만 (별칭 없음/기본값/폴더명과 동일 → 병기 생략)
        assert_eq!(
            App::workspace_display_name(&row("", "/p/binjari")),
            "binjari"
        );
        assert_eq!(
            App::workspace_display_name(&row("default", "/p/binjari")),
            "binjari"
        );
        assert_eq!(
            App::workspace_display_name(&row("binjari", "/p/binjari")),
            "binjari"
        );
        // 별칭 병기 — 레거시의 폴더와 다른 이름은 자동으로 별칭으로 강등
        assert_eq!(
            App::workspace_display_name(&row("예매봇", "/p/binjari")),
            "binjari (예매봇)"
        );
        // 경로 없음 — 별칭만, 그것도 없으면 "~"
        assert_eq!(App::workspace_display_name(&row("예매봇", "")), "예매봇");
        assert_eq!(App::workspace_display_name(&row("", "")), "~");
    }

    /// 리뷰 P2-1 회귀: cwd 조회 키는 **DB의 raw 제목**이다. i18n 렌더된 값("셸 3")을
    /// 넘기면 항상 miss가 되어 프로젝트명 해석이 조용히 실패한다 — warm 워크스페이스가
    /// 실제로 그랬다(session_titles가 렌더 값을 담고 있었다).
    #[test]
    fn cwd_조회는_raw_제목을_키로_쓴다() {
        let catalog = load_catalog("ko-KR");
        let raw = "workspace.spawn.shell 3";
        let panes = vec![(
            raw.to_owned(),
            "/Users/jr/Desktop/Projects/deppy-sijo".to_owned(),
        )];
        // raw로 조회 → cwd 적중 → 프로젝트명 해석 가능
        assert_eq!(
            pane_cwd(&panes, raw),
            Some("/Users/jr/Desktop/Projects/deppy-sijo")
        );
        // 렌더된 값으로 조회 → miss (옛 버그 경로: session_titles가 렌더 값을 담았다)
        let rendered = ui::workspace::display_pane_title(raw, &catalog);
        assert_ne!(
            rendered, raw,
            "ko에서 렌더 값이 raw와 같으면 이 테스트는 무의미"
        );
        assert_eq!(
            pane_cwd(&panes, &rendered),
            None,
            "렌더 값으로 조회가 적중하면 회귀"
        );
        // 빈 cwd는 없는 것으로 취급
        let empty = vec![(raw.to_owned(), String::new())];
        assert_eq!(pane_cwd(&empty, raw), None);
    }

    /// T1: 세션 cwd가 기존 워크스페이스 경로(또는 하위)에 속하는지 판정 — 등록 배너 조건.
    #[test]
    fn 세션_cwd_워크스페이스_소속_판정() {
        let roots = vec![
            PathBuf::from("/Users/jr/Desktop/Projects/deppy-sijo"),
            PathBuf::from("/Users/jr/work"),
        ];
        // 정확히 일치 → 소속
        assert!(cwd_belongs_to_any(Path::new("/Users/jr/work"), &roots));
        // 하위 폴더 → 소속
        assert!(cwd_belongs_to_any(
            Path::new("/Users/jr/Desktop/Projects/deppy-sijo/crates/app"),
            &roots
        ));
        // 무관한 새 폴더 → 미소속 (배너 표시 대상)
        assert!(!cwd_belongs_to_any(
            Path::new("/Users/jr/Desktop/Projects/Crawler"),
            &roots
        ));
        // 접두 문자열만 같은 형제 폴더는 오판하지 않는다 (/a/bc vs /a/b)
        assert!(!cwd_belongs_to_any(
            Path::new("/Users/jr/workbench"),
            &roots
        ));
        // 루트 목록이 비면 항상 미소속
        assert!(!cwd_belongs_to_any(Path::new("/tmp"), &[]));
    }

    /// 활동 패널(warm/유휴)의 pane 이름: 기본 제목은 프로젝트명으로, rename은 그대로.
    #[test]
    fn 활동_pane_이름은_기본제목이면_프로젝트명으로_표시된다() {
        let catalog = load_catalog("ko-KR");
        // 기본 제목 + cwd → 프로젝트(폴더)명
        assert_eq!(
            activity_session_name(
                "workspace.spawn.shell 1",
                Some("/Users/jr/Desktop/Projects/deppy-sijo"),
                &catalog,
                |cwd| crate::agent_detect::project_display_name(
                    cwd,
                    crate::config::SessionNameStyle::Folder
                ),
            ),
            "deppy-sijo"
        );
        // 사용자 rename은 cwd와 무관하게 그대로
        assert_eq!(
            activity_session_name("배포 작업", Some("/tmp/whatever"), &catalog, |cwd| {
                crate::agent_detect::project_display_name(
                    cwd,
                    crate::config::SessionNameStyle::Folder,
                )
            }),
            "배포 작업"
        );
        // cwd 없음/빈 값 → 기본 제목 i18n 렌더로 폴백(기존 동작)
        let fallback = ui::workspace::display_pane_title("workspace.spawn.shell 3", &catalog);
        assert_eq!(
            activity_session_name("workspace.spawn.shell 3", None, &catalog, |cwd| {
                crate::agent_detect::project_display_name(
                    cwd,
                    crate::config::SessionNameStyle::Folder,
                )
            }),
            fallback
        );
        assert_eq!(
            activity_session_name("workspace.spawn.shell 3", Some(""), &catalog, |cwd| {
                crate::agent_detect::project_display_name(
                    cwd,
                    crate::config::SessionNameStyle::Folder,
                )
            }),
            fallback
        );
        // 상대경로(비정상) → 폴백
        assert_eq!(
            activity_session_name(
                "workspace.spawn.shell 3",
                Some("relative/path"),
                &catalog,
                |cwd| crate::agent_detect::project_display_name(
                    cwd,
                    crate::config::SessionNameStyle::Folder
                )
            ),
            fallback
        );
    }

    /// 구분 가능한 최소 MuxUpdated 이벤트 (active_tab 태그로 스냅샷을 식별).
    fn mux_event(tag: &str) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::MuxUpdated {
            snapshot: std::sync::Arc::new(runtime::MuxSnapshot {
                tabs: Vec::new(),
                active_tab: Some(runtime::MuxTabId(tag.to_owned())),
                focused_pane: None,
            }),
        }
    }

    fn mux_tag(e: &runtime::RuntimeEvent) -> Option<&str> {
        match e {
            runtime::RuntimeEvent::MuxUpdated { snapshot } => {
                snapshot.active_tab.as_ref().map(|t| t.0.as_str())
            }
            _ => None,
        }
    }

    fn resource_event(sampled_at_ms: u64) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::ResourceUsage {
            snapshot: runtime::ProcessResourceSnapshot {
                pid: 42,
                sampled_at_ms,
                rss_bytes: sampled_at_ms * 1024,
                cpu_percent: Some(sampled_at_ms as f32),
                high_cpu: false,
                high_rss: false,
            },
            session_usage: Vec::new(),
        }
    }

    fn input_pressure_event(session: u64, queued_bytes: usize) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::PtyInputPressure {
            session: runtime::SessionId(session),
            pressure: runtime::PtyInputPressure {
                attempted_bytes: queued_bytes + 1,
                queued_bytes,
                queued_messages: 1,
                max_bytes: 1024,
                max_messages: 8,
                reason: runtime::PtyInputRejectReason::QueueFull,
            },
        }
    }

    fn viewport_event(session: u64, title: &str) -> runtime::RuntimeEvent {
        runtime::RuntimeEvent::Viewport {
            session: runtime::SessionId(session),
            snapshot: std::sync::Arc::new(terminal::TerminalViewportSnapshot {
                cols: 0,
                rows: 0,
                cursor: terminal::CursorSnapshot {
                    col: 0,
                    row: 0,
                    shape: terminal::CursorShape::Block,
                    visible: false,
                },
                visible_cells: Vec::new().into(),
                dirty_ranges: Vec::new(),
                title: Some(title.to_owned()),
                scroll_offset: 0,
                is_alt_screen: false,
            }),
            bracketed_paste: false,
        }
    }

    fn temp_db_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "deppy-sijo-{name}-{}-{nanos}.sqlite3",
            std::process::id()
        ))
    }

    fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut file_name = path.as_os_str().to_owned();
        file_name.push(suffix);
        PathBuf::from(file_name)
    }

    fn remove_sqlite_files(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sqlite_sidecar(path, "-wal"));
        let _ = std::fs::remove_file(sqlite_sidecar(path, "-shm"));
    }

    struct MemSecretStore(Mutex<HashMap<String, String>>);

    impl MemSecretStore {
        fn new() -> Self {
            Self(Mutex::new(HashMap::new()))
        }

        fn contains(&self, id: &str) -> bool {
            self.0.lock().unwrap().contains_key(id)
        }
    }

    impl secret::SecretStore for MemSecretStore {
        fn set_secret(&self, id: &str, secret: &secret::SecretString) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_owned(), secret.expose().to_owned());
            Ok(())
        }

        fn get_secret(&self, id: &str) -> anyhow::Result<secret::SecretString> {
            let value = self
                .0
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing secret: {id}"))?;
            Ok(secret::SecretString::new(value))
        }

        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }

        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.contains(id))
        }
    }

    #[test]
    fn credentials_service_add_delete_owns_db_and_secret_side_effects() {
        let path = temp_db_path("credentials-service");
        let db = storage::Db::open(&path).unwrap();
        let store = MemSecretStore::new();
        let redaction = secret::RedactionService::new();
        let service = AppCredentialService {
            db: &db,
            secret_store: &store,
            redaction: &redaction,
            workspace_id: "ws-test",
        };

        ui::credentials::CredentialService::add_credential(
            &service,
            ui::credentials::NewCredential {
                provider: "test".to_owned(),
                label: "unit".to_owned(),
                credential_kind: "api_key".to_owned(),
                secret: "sk-test-boundary-secret".to_owned(),
            },
        )
        .unwrap();

        let rows = db.list_credentials().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(store.contains(&rows[0].id));

        ui::credentials::CredentialService::delete_credential(&service, &rows[0].id).unwrap();
        assert!(db.list_credentials().unwrap().is_empty());
        assert!(!store.contains(&rows[0].id));

        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn connectors_oauth_secret_adapter_stores_access_and_refresh_tokens() {
        let store = MemSecretStore::new();
        let redaction = secret::RedactionService::new();
        let service = AppOAuthCredentialStore {
            secret_store: &store,
            redaction: &redaction,
        };
        let token = auth::OAuthToken {
            access_token: secret::SecretString::new("access-token-secret".to_owned()),
            refresh_token: Some(secret::SecretString::new("refresh-token-secret".to_owned())),
            expires_in_secs: Some(3600),
        };

        let stored =
            ui::connectors::OAuthCredentialStore::store_oauth_token(&service, &token).unwrap();

        assert!(store.contains(&stored.id));
        assert!(store.contains(&auth::refresh_entry_id(&stored.id)));
        assert_eq!(stored.masked_hint, "****cret");

        ui::connectors::OAuthCredentialStore::delete_oauth_token(&service, &stored.id).unwrap();
        assert!(!store.contains(&stored.id));
        assert!(!store.contains(&auth::refresh_entry_id(&stored.id)));
    }

    #[test]
    fn workspace_path_to_tree_root는_빈_경로를_desktop_fallback하지_않는다() {
        assert_eq!(App::workspace_path_to_tree_root(None), None);
        assert_eq!(App::workspace_path_to_tree_root(Some(String::new())), None);
        assert_eq!(
            App::workspace_path_to_tree_root(Some("   \t ".to_owned())),
            None
        );
        assert_eq!(
            App::workspace_path_to_tree_root(Some(" /tmp/project ".to_owned())),
            Some(PathBuf::from(" /tmp/project "))
        );
    }

    #[test]
    fn parse_known_hosts_주석_빈줄_손상행_스킵하고_순서보존() {
        let text = "# deppy remote TLS known_hosts\n\
                    127.0.0.1:7777 aa:bb:cc\n\
                    \n\
                    host-only-no-fp\n\
                    [::1]:9000 dd:ee:ff\n";
        let rows = parse_known_hosts(text);
        assert_eq!(
            rows,
            vec![
                ("127.0.0.1:7777".to_owned(), "aa:bb:cc".to_owned()),
                ("[::1]:9000".to_owned(), "dd:ee:ff".to_owned()),
            ]
        );
    }

    #[test]
    fn parse_known_hosts_중복host는_last_wins_소문자정규화() {
        // KnownHosts::load(HashMap)와 동일한 effective view — 낡은 지문을 표시하지 않는다
        let text = "h:1 AA:BB
h:2 cc:dd
h:1 EE:FF
";
        let rows = parse_known_hosts(text);
        assert_eq!(rows.len(), 2);
        assert!(rows.contains(&("h:1".to_owned(), "ee:ff".to_owned())));
        assert!(rows.contains(&("h:2".to_owned(), "cc:dd".to_owned())));
    }

    #[test]
    fn approval_watcher_empty_db는_repaint를_예약하지_않는다() {
        let path = temp_db_path("approval-empty");
        let db = storage::Db::open(&path).unwrap();
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.set_request_repaint_callback(move |info| {
            let _ = tx.send(info.delay);
        });
        let poll_requested = Arc::new(AtomicBool::new(false));
        let mut watcher = ApprovalWatcher::spawn(
            path.clone(),
            ctx,
            poll_requested.clone(),
            std::time::Duration::from_millis(20),
        );

        std::thread::sleep(std::time::Duration::from_millis(90));

        assert!(rx.try_recv().is_err());
        assert!(!poll_requested.load(Ordering::Acquire));
        watcher.stop();
        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn approval_watcher_pending_삽입시_ui를_깨운다() {
        let path = temp_db_path("approval-pending");
        let db = storage::Db::open(&path).unwrap();
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.set_request_repaint_callback(move |info| {
            let _ = tx.send(info.delay);
        });
        let poll_requested = Arc::new(AtomicBool::new(false));
        let mut watcher = ApprovalWatcher::spawn(
            path.clone(),
            ctx,
            poll_requested.clone(),
            std::time::Duration::from_millis(20),
        );

        db.insert_pending_approval("req-1", "srv", "tool", "{}", None, 100, None)
            .unwrap();

        let delay = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(delay, std::time::Duration::ZERO);
        assert!(poll_requested.load(Ordering::Acquire));
        watcher.stop();
        drop(db);
        remove_sqlite_files(&path);
    }

    #[test]
    fn coalesce_moves_latest_mux_to_front() {
        let mut events = vec![
            mux_event("a"),
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Running,
            },
            mux_event("b"),
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(1),
                exit_code: None,
            },
            mux_event("c"),
        ];

        coalesce_mux_updated(&mut events);

        // 최신 mux(c)만 남아 맨 앞으로. 이전 mux(a, b) 제거. lifecycle는 순서 보존.
        assert_eq!(events.len(), 3);
        assert_eq!(mux_tag(&events[0]), Some("c"));
        assert!(matches!(
            events[1],
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Running,
            }
        ));
        assert!(matches!(
            events[2],
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(1),
                exit_code: None,
            }
        ));
    }

    #[test]
    fn coalesce_dedups_status_per_session() {
        // 같은 세션의 status churn → 세션별 최신 1개만. 유지분 상대 순서 보존.
        let mut events = vec![
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Running,
            },
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Waiting,
            },
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(2),
                status: runtime::SessionStatus::Running,
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 2);
        // 세션1은 최신(Waiting)만, 세션2는 그대로. 순서: 세션1 → 세션2.
        assert!(matches!(
            events[0],
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(1),
                status: runtime::SessionStatus::Waiting,
            }
        ));
        assert!(matches!(
            events[1],
            runtime::RuntimeEvent::SessionStatusChanged {
                session: runtime::SessionId(2),
                status: runtime::SessionStatus::Running,
            }
        ));
    }

    #[test]
    fn coalesce_dedups_status_view_per_session() {
        let view = |status| {
            runtime::SessionStatusView::detected(
                status,
                runtime::StatusSource::StreamRegex,
                None,
                None,
            )
        };
        let mut events = vec![
            runtime::RuntimeEvent::SessionStatusViewChanged {
                session: runtime::SessionId(1),
                view: view(runtime::SessionStatus::Running),
            },
            runtime::RuntimeEvent::SessionStatusViewChanged {
                session: runtime::SessionId(1),
                view: view(runtime::SessionStatus::Waiting),
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            runtime::RuntimeEvent::SessionStatusViewChanged {
                session: runtime::SessionId(1),
                view
            } if view.status == runtime::SessionStatus::Waiting
        ));
    }

    #[test]
    fn coalesce_dedups_resource_usage_to_latest() {
        let mut events = vec![
            resource_event(1),
            runtime::RuntimeEvent::ShellSpawned {
                session: runtime::SessionId(7),
            },
            resource_event(2),
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(7),
                exit_code: Some(0),
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            runtime::RuntimeEvent::ShellSpawned {
                session: runtime::SessionId(7)
            }
        ));
        assert!(matches!(
            &events[1],
            runtime::RuntimeEvent::ResourceUsage { snapshot, .. }
                if snapshot.sampled_at_ms == 2
        ));
        assert!(matches!(
            &events[2],
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(7),
                exit_code: Some(0)
            }
        ));
    }

    #[test]
    fn coalesce_dedups_input_pressure_per_session() {
        let mut events = vec![
            input_pressure_event(1, 10),
            input_pressure_event(2, 20),
            input_pressure_event(1, 30),
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(1),
                exit_code: Some(0),
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            runtime::RuntimeEvent::PtyInputPressure { session, pressure }
                if *session == runtime::SessionId(2) && pressure.queued_bytes == 20
        ));
        assert!(matches!(
            &events[1],
            runtime::RuntimeEvent::PtyInputPressure { session, pressure }
                if *session == runtime::SessionId(1) && pressure.queued_bytes == 30
        ));
        assert!(matches!(
            &events[2],
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(1),
                exit_code: Some(0)
            }
        ));
    }

    #[test]
    fn coalesce_dedups_viewport_per_session() {
        let mut events = vec![
            viewport_event(1, "old"),
            viewport_event(2, "other"),
            viewport_event(1, "latest"),
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            runtime::RuntimeEvent::Viewport { session, snapshot, .. }
                if *session == runtime::SessionId(2)
                    && snapshot.title.as_deref() == Some("other")
        ));
        assert!(matches!(
            &events[1],
            runtime::RuntimeEvent::Viewport { session, snapshot, .. }
                if *session == runtime::SessionId(1)
                    && snapshot.title.as_deref() == Some("latest")
        ));
    }

    #[test]
    fn coalesce_exit_stays_after_mux_for_replay() {
        // [MuxUpdated(세션X 도입), SessionExited(X)] → coalesce 후에도 exit이 mux 뒤에.
        // (mux가 맨 앞으로 가므로 replay 시 X를 먼저 확립하고 exit이 적용됨.)
        let mut events = vec![
            mux_event("x"),
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(9),
                exit_code: Some(0),
            },
        ];

        coalesce_mux_updated(&mut events);

        assert_eq!(events.len(), 2);
        assert_eq!(mux_tag(&events[0]), Some("x"));
        assert!(matches!(
            events[1],
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(9),
                exit_code: Some(0),
            }
        ));
    }

    #[test]
    fn coalesce_noop_without_mux() {
        let mut events = vec![
            runtime::RuntimeEvent::ShellSpawned {
                session: runtime::SessionId(7),
            },
            runtime::RuntimeEvent::SessionExited {
                session: runtime::SessionId(7),
                exit_code: Some(0),
            },
        ];
        coalesce_mux_updated(&mut events);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn warm_auto_suspend_candidates_respect_timeout_and_order() {
        let now = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(60);
        let ids = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let mut backgrounded = HashMap::new();
        backgrounded.insert("a".to_owned(), now - std::time::Duration::from_secs(61));
        backgrounded.insert("b".to_owned(), now - std::time::Duration::from_secs(59));
        backgrounded.insert("c".to_owned(), now - std::time::Duration::from_secs(120));

        assert_eq!(
            expired_warm_workspace_ids(&ids, |id| backgrounded.get(id).copied(), now, timeout),
            vec!["a".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn global_terminal_cache_budget_is_divided_across_resident_runtimes() {
        const MIB: usize = 1024 * 1024;
        assert_eq!(per_runtime_cache_budget_bytes(128, 1), 128 * MIB);
        assert_eq!(per_runtime_cache_budget_bytes(128, 2), 64 * MIB);
        assert_eq!(per_runtime_cache_budget_bytes(128, 4), 32 * MIB);

        let share = per_runtime_cache_budget_bytes(128, 3);
        assert!(share * 3 <= 128 * MIB);
        assert!((128 * MIB) - share * 3 < 3, "나눗셈 나머지만 미배정");

        // 방어적 0 count는 active runtime 하나로 취급하고, 비정상 0MB도 1MiB로 제한한다.
        assert_eq!(per_runtime_cache_budget_bytes(32, 0), 32 * MIB);
        assert_eq!(per_runtime_cache_budget_bytes(0, 12), MIB);
    }

    #[test]
    fn only_sampled_single_process_low_cpu_shells_are_auto_suspendable() {
        let s1 = runtime::SessionId(1);
        let s2 = runtime::SessionId(2);
        let sample = |session, process_count, cpu_percent| runtime::SessionResourceUsage {
            session,
            pid: Some(session.0 as u32),
            process_group: Some(session.0 as u32),
            identity_source: runtime::ProcessIdentitySource::PortablePty,
            sampled_at_ms: 1,
            process_count,
            rss_bytes: 1024,
            cpu_percent,
            high_cpu: false,
            high_rss: false,
        };
        let sessions = [s1, s2];
        let idle = [sample(s1, 1, Some(0.0)), sample(s2, 1, Some(0.5))];
        assert!(shell_sessions_are_idle(&sessions, &idle, |_| false));
        assert!(!shell_sessions_are_idle(&sessions, &idle, |s| s == s2));
        assert!(!shell_sessions_are_idle(&sessions, &idle[..1], |_| false));

        let child_work = [sample(s1, 2, Some(0.0)), sample(s2, 1, Some(0.0))];
        assert!(!shell_sessions_are_idle(&sessions, &child_work, |_| false));
        let busy = [sample(s1, 1, Some(1.1)), sample(s2, 1, Some(0.0))];
        assert!(!shell_sessions_are_idle(&sessions, &busy, |_| false));
        let unsampled_cpu = [sample(s1, 1, None), sample(s2, 1, Some(0.0))];
        assert!(!shell_sessions_are_idle(&sessions, &unsampled_cpu, |_| {
            false
        }));
    }

    #[test]
    fn warm_eviction은_live_workspace를_건너뛴다() {
        let ids = vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
        ];
        // 상한 2, 초과 2 — 가장 오래된 a부터 고르되 live(a, c)는 건너뛴다
        let live: std::collections::HashSet<&str> = ["a", "c"].into();
        assert_eq!(
            warm_eviction_candidates(&ids, 2, |id| live.contains(id)),
            vec!["b".to_owned(), "d".to_owned()]
        );
        // 전부 live면 아무것도 축출하지 않는다 (상한 초과 허용 — 작업 보호)
        assert_eq!(
            warm_eviction_candidates(&ids, 2, |_| true),
            Vec::<String>::new()
        );
        // 초과 없음 → 빈 결과
        assert_eq!(
            warm_eviction_candidates(&ids, 4, |_| false),
            Vec::<String>::new()
        );
        // live 아닌 것이 초과분보다 많아도 초과분만큼만 축출
        assert_eq!(
            warm_eviction_candidates(&ids, 3, |_| false),
            vec!["a".to_owned()]
        );
    }

    #[test]
    fn workspace_is_live는_spawn대기와_초기유예를_존중한다() {
        let d = std::time::Duration::from_secs;
        // tracker가 live면 무조건 live
        assert!(workspace_is_live(true, true, 0, d(999)));
        // spawn 응답 대기 중이면 live (mux 관측과 무관)
        assert!(workspace_is_live(false, true, 1, d(999)));
        // 첫 MuxUpdated 관측 전 + 유예 내 → live (restore 이벤트 미도착 창)
        assert!(workspace_is_live(false, false, 0, d(1)));
        // 유예가 지나면 빈 workspace로 취급 — suspend 가능
        assert!(!workspace_is_live(false, false, 0, d(11)));
        // mux 관측 후 세션 없음 → suspend 가능
        assert!(!workspace_is_live(false, true, 0, d(1)));
    }

    #[test]
    fn live_warm_예상치는_기존_target_전환과_신규_전환을_구분한다() {
        assert_eq!(projected_live_warm_count(4, true, true), 4);
        assert_eq!(projected_live_warm_count(4, false, true), 5);
        assert_eq!(projected_live_warm_count(4, false, false), 4);
    }

    #[test]
    fn live_세션_추적은_mux와_exited를_반영한다() {
        use std::sync::Arc;
        let mut tracker = LiveSessionTracker::default();
        assert!(!tracker.has_live(), "빈 workspace는 live 아님");

        let s1 = runtime::SessionId(1);
        let mux = |sessions: &[runtime::SessionId]| runtime::RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: runtime::MuxTabId::new(),
                    title: "t".into(),
                    layout: runtime::LayoutNode::Pane(runtime::MuxPaneId::new()),
                    panes: sessions
                        .iter()
                        .map(|s| runtime::PaneSnapshot {
                            id: runtime::MuxPaneId::new(),
                            session_id: Some(*s),
                            title: "p".into(),
                            persistent_session_id: None,
                        })
                        .collect(),
                }],
                active_tab: None,
                focused_pane: None,
            }),
        };

        // 세션 attach → live
        tracker.observe(&mux(&[s1]));
        assert!(tracker.has_live());
        assert!(
            tracker.live_shell_sessions().is_none(),
            "spawn 종류 확인 전은 보호"
        );
        tracker.observe(&runtime::RuntimeEvent::ShellSpawned { session: s1 });
        assert_eq!(tracker.live_shell_sessions(), Some(vec![s1]));

        // Exited → live 아님 (pane은 남아 있어도 프로세스는 죽음 — agent 결과 pane)
        tracker.observe(&runtime::RuntimeEvent::SessionExited {
            session: s1,
            exit_code: Some(0),
        });
        assert!(!tracker.has_live());
        assert!(tracker.live_shell_sessions().is_none());

        // pane 제거 MuxUpdated → exited 집합도 정리(유계)
        tracker.observe(&mux(&[]));
        assert!(tracker.exited_sessions.is_empty());
        assert!(!tracker.has_live());
    }

    #[test]
    fn agent가_섞인_live_session은_idle_shell_suspend에서_제외된다() {
        use std::sync::Arc;
        let shell = runtime::SessionId(1);
        let agent = runtime::SessionId(2);
        let mut tracker = LiveSessionTracker::default();
        tracker.observe(&runtime::RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: runtime::MuxTabId::new(),
                    title: "t".into(),
                    layout: runtime::LayoutNode::Pane(runtime::MuxPaneId::new()),
                    panes: [shell, agent]
                        .into_iter()
                        .map(|session| runtime::PaneSnapshot {
                            id: runtime::MuxPaneId::new(),
                            session_id: Some(session),
                            title: "p".into(),
                            persistent_session_id: None,
                        })
                        .collect(),
                }],
                active_tab: None,
                focused_pane: None,
            }),
        });
        tracker.observe(&runtime::RuntimeEvent::ShellSpawned { session: shell });
        tracker.observe(&runtime::RuntimeEvent::AgentSpawned { session: agent });
        assert!(tracker.has_live());
        assert!(tracker.live_shell_sessions().is_none());
    }

    /// PR-A2 회귀 방지: 재시작 시 archived 복원된 세션은 SessionRestored로 오고,
    /// SessionExited와 동일하게 생존 추적에서 제외돼야 한다 (그러지 않으면 복원된
    /// agent pane 때문에 auto-suspend/warm 축출이 영구 무력화됨 — codex 리뷰 P1).
    #[test]
    fn session_restored도_live_추적에서_제외된다() {
        use std::sync::Arc;
        let mut tracker = LiveSessionTracker::default();
        let s1 = runtime::SessionId(1);
        let mux = |sessions: &[runtime::SessionId]| runtime::RuntimeEvent::MuxUpdated {
            snapshot: Arc::new(runtime::MuxSnapshot {
                tabs: vec![runtime::TabSnapshot {
                    id: runtime::MuxTabId::new(),
                    title: "t".into(),
                    layout: runtime::LayoutNode::Pane(runtime::MuxPaneId::new()),
                    panes: sessions
                        .iter()
                        .map(|s| runtime::PaneSnapshot {
                            id: runtime::MuxPaneId::new(),
                            session_id: Some(*s),
                            title: "p".into(),
                            persistent_session_id: None,
                        })
                        .collect(),
                }],
                active_tab: None,
                focused_pane: None,
            }),
        };
        // 복원된 pane(세션 있음) — SessionExited 없이 SessionRestored만 온다
        tracker.observe(&mux(&[s1]));
        assert!(
            tracker.has_live(),
            "SessionRestored 관측 전에는 live로 보임"
        );
        tracker.observe(&runtime::RuntimeEvent::SessionRestored {
            session: s1,
            exit_code: Some(0),
        });
        assert!(
            !tracker.has_live(),
            "복원된 exited 세션은 live 아님 — auto-suspend 정상 동작"
        );
    }

    /// env/API 프로젝트 행 캐시의 TTL 판정 — 캐시 없음/TTL 경과면 재계산, 그 안이면 재사용.
    #[test]
    fn env_api_cache_ttl_judgement() {
        let base = std::time::Instant::now();
        // 캐시 없음(명시 무효화 직후) → 재계산.
        assert!(App::env_api_cache_expired(None, base));
        // 방금 계산 → 재사용.
        assert!(!App::env_api_cache_expired(Some(base), base));
        // TTL(1s) 직전 → 재사용.
        assert!(!App::env_api_cache_expired(
            Some(base),
            base + std::time::Duration::from_millis(999)
        ));
        // TTL 경과 → 재계산.
        assert!(App::env_api_cache_expired(
            Some(base),
            base + std::time::Duration::from_millis(1000)
        ));
        // 계산 시각이 now보다 뒤(시계 보정 등) → saturating으로 0 취급, 재사용.
        assert!(!App::env_api_cache_expired(
            Some(base + std::time::Duration::from_secs(5)),
            base
        ));
    }
}
