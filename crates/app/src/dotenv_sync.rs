//! 프로젝트 루트의 dotenv 파일들을 설정 › 환경(env profile)으로 자동 동기화한다 (2026-07-07).
//!
//! - workspace 활성화 시 루트의 `.env` → `.env.local`을 관례 순서로 **병합**해 파싱하고
//!   (뒤 파일이 같은 키를 덮어씀 — dotenv 표준 우선순위) kind=`dotenv` profile로 upsert한다.
//!   `.env.development` 같은 모드별 파일은 앱이 실행 모드를 모르므로 읽지 않는다.
//!   secret으로 보이는 키(API/SECRET/TOKEN/…)는 값이 DB가 아닌 **OS keyring**(credential)에
//!   저장되고, 나머지는 plain으로 저장된다. 병합 결과에서 사라진 키는 profile에서도 지운다.
//! - dotenv 파일이 source of truth — dotenv profile의 해당 키를 UI에서 고쳐도 다음 동기화가
//!   파일 값으로 되돌린다(다른 profile은 건드리지 않음).
//! - dotenv 파일이 하나도 없으면 아무것도 만들지 않고, 기존 dotenv profile은 그대로 둔다
//!   (일시적 체크아웃 차이로 저장된 환경이 사라지지 않게).

use std::io::{Read as _, Write as _};
use std::path::Path;

use crate::env::EnvValue;

/// dotenv 자동 profile의 kind. 환경 UI에는 일반 profile처럼 보인다.
pub const DOTENV_PROFILE_KIND: &str = "dotenv";
/// dotenv 자동 profile 이름.
pub const DOTENV_PROFILE_NAME: &str = ".env";
/// 스캔·병합할 dotenv 파일 이름(관례 순서 — 뒤 파일이 같은 키를 덮어씀).
pub const DOTENV_FILE_NAMES: [&str; 2] = runtime::dotenv::DOTENV_FILE_NAMES;

pub use runtime::dotenv::is_secret_key;
#[cfg(test)]
use runtime::dotenv::parse_dotenv;
use runtime::dotenv::{
    DOTENV_ENTRIES_MAX, DOTENV_KEY_BYTES_MAX, DOTENV_TOTAL_BYTES_MAX, DOTENV_VALUE_BYTES_MAX,
    parse_dotenv_bounded,
};

/// Comments and blank lines are retained by the editor, so cap them separately from parsed vars.
const DOTENV_LINES_MAX: usize = 8_192;
/// `.gitignore` is control text. Larger repositories keep generated ignore data elsewhere.
const GITIGNORE_BYTES_MAX: usize = 256 * 1024;

/// `.gitignore` 잠금 재시도 상한/간격 — 고전적 flock+fork 레이스를 흡수한다.
/// flock은 파일이 아니라 **open file description**에 붙고, 그 description을 가리키는
/// 마지막 fd가 close될 때 풀린다. std가 여는 fd는 전부 O_CLOEXEC지만 CLOEXEC는
/// **exec 시점**에만 적용되므로, 우리가 이 잠금을 쥔 채로 프로세스의 *다른 스레드*가
/// fork()하면(PTY spawn, git CLI, port_inventory의 pre_exec 기반 Command 등 — 앱에서
/// 상시 일어나는 일) 그 fd가 자식에게도 복제된다. 우리가 곧바로 close해도 자식이
/// exec/exit로 자기 사본을 닫을 때까지는 동일 open file description이 살아있어,
/// 바로 이어지는 재-lock 시도가 일시적으로 WouldBlock을 본다.
///
/// 2026-08-14 실증: 전체 스위트를 `--test-threads=64`로 병렬 실행하면 테스트마다
/// uuid로 고유한 inode인데도 `dotenv_gitignore_lock_failed`가 간헐 실패했다
/// (`gitignore_reader_accepts_exact_and_rejects_plus_one_and_invalid_utf8`). 진단
/// 재시도로 실측: 1회 WouldBlock 후 31ms 만에 해소(스레드 스케줄 지연 포함, 즉 자식의
/// close 자체는 더 빠르다). 우리 자신의 누수라면 재시도해도 계속 WouldBlock으로
/// 남으므로, 실측치의 8배 이상 여유를 두고 상한을 넘기면 기존 fail-closed 오류를
/// 그대로 낸다.
const GITIGNORE_LOCK_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);
const GITIGNORE_LOCK_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

fn configure_no_follow(options: &mut std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        #[cfg(target_os = "macos")]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(0x20_000 | 0x800);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

fn read_dotenv_file_app_bounded(
    path: &Path,
    remaining_bytes: &mut usize,
) -> anyhow::Result<Option<String>> {
    anyhow::ensure!(
        *remaining_bytes <= DOTENV_TOTAL_BYTES_MAX,
        "dotenv_total_bytes_exceeded"
    );
    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("dotenv_read_failed"),
    };
    anyhow::ensure!(
        before.file_type().is_file() && !before.file_type().is_symlink(),
        "dotenv_file_type_invalid"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let mut file = options
        .open(path)
        .map_err(|_| anyhow::anyhow!("dotenv_read_failed"))?;
    let opened = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("dotenv_metadata_failed"))?;
    anyhow::ensure!(opened.is_file(), "dotenv_file_type_invalid");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        anyhow::ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino(),
            "dotenv_file_changed"
        );
    }
    anyhow::ensure!(
        opened.len() <= *remaining_bytes as u64,
        "dotenv_total_bytes_exceeded"
    );
    let probe = remaining_bytes
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("dotenv_total_bytes_exceeded"))?;
    let mut bytes = Vec::with_capacity((opened.len() as usize).min(probe));
    std::io::Read::by_ref(&mut file)
        .take(probe as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("dotenv_read_failed"))?;
    anyhow::ensure!(
        bytes.len() <= *remaining_bytes,
        "dotenv_total_bytes_exceeded"
    );
    let after = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("dotenv_metadata_failed"))?;
    anyhow::ensure!(after.len() == bytes.len() as u64, "dotenv_file_changed");
    *remaining_bytes -= bytes.len();
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("dotenv_utf8_invalid"))
}

fn read_dotenv_merged_app_bounded(root: &Path) -> anyhow::Result<Option<Vec<(String, String)>>> {
    let mut merged: Vec<(String, String)> = Vec::new();
    let mut positions = std::collections::HashMap::<String, usize>::new();
    let mut remaining_bytes = DOTENV_TOTAL_BYTES_MAX;
    let mut remaining_entries = DOTENV_ENTRIES_MAX;
    let mut found = false;

    for name in DOTENV_FILE_NAMES {
        let Some(content) = read_dotenv_file_app_bounded(&root.join(name), &mut remaining_bytes)?
        else {
            continue;
        };
        found = true;
        let parsed = parse_dotenv_bounded(&content, remaining_entries)?;
        remaining_entries -= parsed.len();
        for (key, value) in parsed {
            if let Some(index) = positions.get(&key).copied() {
                merged[index].1 = value;
            } else {
                anyhow::ensure!(merged.len() < DOTENV_ENTRIES_MAX, "dotenv_entries_exceeded");
                positions.insert(key.clone(), merged.len());
                merged.push((key, value));
            }
        }
    }
    Ok(found.then_some(merged))
}

fn collect_dotenv_lines_bounded(content: &str) -> anyhow::Result<Vec<String>> {
    let mut lines = Vec::with_capacity(128);
    for line in content.lines() {
        anyhow::ensure!(lines.len() < DOTENV_LINES_MAX, "dotenv_lines_exceeded");
        lines.push(line.to_owned());
    }
    Ok(lines)
}

/// 동기화 결과 요약 (로그/알림용).
#[derive(Debug, Default, PartialEq)]
pub struct DotenvSyncReport {
    pub upserted: usize,
    pub removed: usize,
    /// 보호할 수 없어 동기화에서 제외한 키(2026-08-21). 값은 담지 않는다 — 키 이름만.
    /// 상한은 `DOTENV_SKIPPED_KEYS_MAX`.
    pub skipped_keys: Vec<String>,
}

/// 리포트에 담는 제외 키의 상한. 넘으면 개수만 세고 이름은 더 담지 않는다.
pub const DOTENV_SKIPPED_KEYS_MAX: usize = 32;

/// Persistence projection used by dotenv orchestration. Storage rows must be mapped at the app
/// composition root rather than crossing into this module.
pub struct DotenvProfile {
    pub id: String,
    pub kind: String,
}

/// Environment variable projection used by dotenv orchestration.
pub struct DotenvVariable {
    pub key: String,
    pub value: EnvValue,
}

/// Logical credential pointer resolved by the app-owned repository adapter. The physical
/// coordinate is intentionally non-Clone/non-Serialize and never appears in Debug output.
pub struct DotenvSecretLocation {
    keyring_service: String,
    physical_pointer: String,
}

impl DotenvSecretLocation {
    pub fn new(keyring_service: String, physical_pointer: String) -> Self {
        Self {
            keyring_service,
            physical_pointer,
        }
    }
}

impl std::fmt::Debug for DotenvSecretLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DotenvSecretLocation")
            .field("coordinate", &"REDACTED")
            .finish()
    }
}

/// Metadata for a newly staged dotenv credential. The masked hint and workspace association are
/// private capabilities, so this DTO is non-Clone/non-Serialize and fully redacted in Debug.
pub struct DotenvCredentialDraft {
    id: String,
    provider: String,
    label: String,
    credential_kind: String,
    masked_hint: Option<String>,
    workspace_id: Option<String>,
}

impl DotenvCredentialDraft {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn credential_kind(&self) -> &str {
        &self.credential_kind
    }

    pub fn masked_hint(&self) -> Option<&str> {
        self.masked_hint.as_deref()
    }

    pub fn workspace_id(&self) -> Option<&str> {
        self.workspace_id.as_deref()
    }
}

impl std::fmt::Debug for DotenvCredentialDraft {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DotenvCredentialDraft")
            .field("credential", &"REDACTED")
            .finish()
    }
}

/// App-owned persistence port for dotenv orchestration. Implementations map concrete storage rows
/// at the composition root and must enforce the caller-provided scan limits before returning.
pub trait DotenvRepository {
    fn credential_secret_location(
        &mut self,
        credential_id: &str,
    ) -> anyhow::Result<Option<DotenvSecretLocation>>;
    fn acknowledge_physical_secret_slot_deleted(
        &mut self,
        logical_id: &str,
        physical_slot: &str,
    ) -> anyhow::Result<()>;
    fn register_physical_secret_slot_staging(
        &mut self,
        logical_id: &str,
        physical_slot: &str,
    ) -> anyhow::Result<()>;
    fn insert_credential_with_secret_slot(
        &mut self,
        draft: &DotenvCredentialDraft,
        physical_slot: &str,
    ) -> anyhow::Result<()>;
    fn publish_credential_secret_slot_cas(
        &mut self,
        logical_id: &str,
        expected_previous_pointer: &str,
        physical_slot: &str,
        masked_hint: Option<&str>,
    ) -> anyhow::Result<bool>;
    fn delete_credential_if_unused_cas(
        &mut self,
        logical_id: &str,
        expected_pointer: &str,
    ) -> anyhow::Result<bool>;
    fn list_env_profiles(
        &mut self,
        workspace_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DotenvProfile>>;
    fn insert_env_profile(
        &mut self,
        workspace_id: &str,
        name: &str,
        kind: &str,
    ) -> anyhow::Result<String>;
    fn list_env_vars(
        &mut self,
        profile_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DotenvVariable>>;
    fn list_dotenv_owned_credential_ids(
        &mut self,
        limit: usize,
    ) -> anyhow::Result<std::collections::HashSet<String>>;
    fn plain_env_value_allowed(&mut self, key: &str, value: &str) -> bool;
    fn upsert_env_var(
        &mut self,
        profile_id: &str,
        key: &str,
        value: &EnvValue,
    ) -> anyhow::Result<()>;
    fn delete_env_var(&mut self, profile_id: &str, key: &str) -> anyhow::Result<()>;
    fn delete_env_profile(&mut self, profile_id: &str) -> anyhow::Result<()>;
}

const DOTENV_PROFILE_SCAN_MAX: usize = 256;
const DOTENV_VARIABLE_SCAN_MAX: usize = 4_096;

/// Bounded, storage-neutral dotenv input prepared from the workspace files.
///
/// Values may contain credentials, so this plan is deliberately non-Clone/non-Serialize and its
/// Debug output reports only bounded cardinality. The worker can now distinguish source I/O and
/// parsing failures from persistence/keyring failures without retaining raw dotenv contents.
pub struct DotenvSyncPlan {
    entries: Vec<(String, String)>,
    value_bytes: usize,
}

impl std::fmt::Debug for DotenvSyncPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DotenvSyncPlan")
            .field("entries", &self.entries.len())
            .field("value_bytes", &self.value_bytes)
            .finish()
    }
}

impl DotenvSyncPlan {
    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn value_bytes(&self) -> usize {
        self.value_bytes
    }
}

const ERROR_SECRET_LOCATION_READ: &str = "dotenv_secret_location_read_failed";
const ERROR_SECRET_LOCATION_MISSING: &str = "dotenv_secret_location_missing";
const ERROR_SECRET_LOCATION_SERVICE: &str = "dotenv_secret_location_service_invalid";
const ERROR_SECRET_LOCATION_LOGICAL: &str = "dotenv_secret_location_logical_invalid";
const ERROR_SECRET_LOCATION_PHYSICAL: &str = "dotenv_secret_location_physical_invalid";
const ERROR_SECRET_LOCATION_OWNER: &str = "dotenv_secret_location_owner_invalid";
const ERROR_SECRET_STAGE_PLAN: &str = "dotenv_secret_stage_plan_failed";
const ERROR_SECRET_LEDGER_STAGE: &str = "dotenv_secret_ledger_stage_failed";
const ERROR_SECRET_BUNDLE_STAGE: &str = "dotenv_secret_bundle_stage_failed";
const ERROR_SECRET_BUNDLE_READ: &str = "dotenv_secret_bundle_read_failed";
const ERROR_SECRET_BUNDLE_DELETE: &str = "dotenv_secret_bundle_delete_failed";
const ERROR_SECRET_LEDGER_ACK: &str = "dotenv_secret_ledger_ack_failed";
const ERROR_SECRET_CREATE_PUBLISH: &str = "dotenv_secret_create_publish_failed";
const ERROR_SECRET_ROTATE_PUBLISH: &str = "dotenv_secret_rotate_publish_failed";
const ERROR_SECRET_ROTATE_STALE: &str = "dotenv_secret_rotate_stale";
const ERROR_SECRET_DELETE_CAS: &str = "dotenv_secret_delete_cas_failed";
const ERROR_SECRET_ENV_BIND: &str = "dotenv_secret_env_bind_failed";
const ERROR_SECRET_REDACTION: &str = "dotenv_secret_redaction_unavailable";
const ERROR_SECRET_INTERNAL: &str = "dotenv_secret_internal_failed";
const ERROR_DOTENV_BYTES: &str = "dotenv_total_bytes_exceeded";

fn static_secret_error(code: &'static str) -> anyhow::Error {
    anyhow::anyhow!(code)
}

fn secret_error_code(error: &anyhow::Error) -> &'static str {
    match error.to_string().as_str() {
        ERROR_SECRET_LOCATION_READ => ERROR_SECRET_LOCATION_READ,
        ERROR_SECRET_LOCATION_MISSING => ERROR_SECRET_LOCATION_MISSING,
        ERROR_SECRET_LOCATION_SERVICE => ERROR_SECRET_LOCATION_SERVICE,
        ERROR_SECRET_LOCATION_LOGICAL => ERROR_SECRET_LOCATION_LOGICAL,
        ERROR_SECRET_LOCATION_PHYSICAL => ERROR_SECRET_LOCATION_PHYSICAL,
        ERROR_SECRET_LOCATION_OWNER => ERROR_SECRET_LOCATION_OWNER,
        ERROR_SECRET_STAGE_PLAN => ERROR_SECRET_STAGE_PLAN,
        ERROR_SECRET_LEDGER_STAGE => ERROR_SECRET_LEDGER_STAGE,
        ERROR_SECRET_BUNDLE_STAGE => ERROR_SECRET_BUNDLE_STAGE,
        ERROR_SECRET_BUNDLE_READ => ERROR_SECRET_BUNDLE_READ,
        ERROR_SECRET_BUNDLE_DELETE => ERROR_SECRET_BUNDLE_DELETE,
        ERROR_SECRET_LEDGER_ACK => ERROR_SECRET_LEDGER_ACK,
        ERROR_SECRET_CREATE_PUBLISH => ERROR_SECRET_CREATE_PUBLISH,
        ERROR_SECRET_ROTATE_PUBLISH => ERROR_SECRET_ROTATE_PUBLISH,
        ERROR_SECRET_ROTATE_STALE => ERROR_SECRET_ROTATE_STALE,
        ERROR_SECRET_DELETE_CAS => ERROR_SECRET_DELETE_CAS,
        ERROR_SECRET_ENV_BIND => ERROR_SECRET_ENV_BIND,
        ERROR_SECRET_REDACTION => ERROR_SECRET_REDACTION,
        _ => ERROR_SECRET_INTERNAL,
    }
}

fn warn_secret_failure(phase: &'static str, error_code: &'static str) {
    tracing::warn!(
        kind = "dotenv_secret",
        phase,
        error_code,
        "dotenv secret lifecycle failed"
    );
}

/// Validated logical-to-physical capability. Coordinates are intentionally omitted from Debug and
/// error output; only the fixed keyring service and owned versioned slot are accepted.
struct ResolvedSecretSlot {
    logical: secret::LogicalCredentialId,
    physical: secret::PhysicalSecretSlot,
}

fn resolve_secret_slot(
    repository: &mut dyn DotenvRepository,
    credential_id: &str,
) -> anyhow::Result<ResolvedSecretSlot> {
    let location = repository
        .credential_secret_location(credential_id)
        .map_err(|_| static_secret_error(ERROR_SECRET_LOCATION_READ))?
        .ok_or_else(|| static_secret_error(ERROR_SECRET_LOCATION_MISSING))?;
    if location.keyring_service != secret::KEYRING_SERVICE {
        return Err(static_secret_error(ERROR_SECRET_LOCATION_SERVICE));
    }
    let logical = secret::LogicalCredentialId::new(credential_id.to_owned())
        .map_err(|_| static_secret_error(ERROR_SECRET_LOCATION_LOGICAL))?;
    let physical = secret::PhysicalSecretSlot::parse(location.physical_pointer)
        .map_err(|_| static_secret_error(ERROR_SECRET_LOCATION_PHYSICAL))?;
    if !physical.belongs_to(&logical) {
        return Err(static_secret_error(ERROR_SECRET_LOCATION_OWNER));
    }
    Ok(ResolvedSecretSlot { logical, physical })
}

fn cleanup_secret_slot(
    repository: &mut dyn DotenvRepository,
    secret_store: &dyn secret::SecretStore,
    slot: &ResolvedSecretSlot,
) -> anyhow::Result<()> {
    secret::delete_secret_bundle(secret_store, &slot.physical)
        .map_err(|_| static_secret_error(ERROR_SECRET_BUNDLE_DELETE))?;
    repository
        .acknowledge_physical_secret_slot_deleted(slot.logical.as_str(), slot.physical.as_str())
        .map_err(|_| static_secret_error(ERROR_SECRET_LEDGER_ACK))?;
    Ok(())
}

fn cleanup_secret_slot_best_effort(
    repository: &mut dyn DotenvRepository,
    secret_store: &dyn secret::SecretStore,
    slot: &ResolvedSecretSlot,
    phase: &'static str,
) {
    if secret::delete_secret_bundle(secret_store, &slot.physical).is_err() {
        // The staging/orphan ledger row remains durable. Startup reconciliation can retry the
        // exact idempotent bundle deletion without enumerating or guessing keyring usernames.
        warn_secret_failure(phase, ERROR_SECRET_BUNDLE_DELETE);
        return;
    }
    if repository
        .acknowledge_physical_secret_slot_deleted(slot.logical.as_str(), slot.physical.as_str())
        .is_err()
    {
        warn_secret_failure(phase, ERROR_SECRET_LEDGER_ACK);
    }
}

fn stage_access_only_secret(
    repository: &mut dyn DotenvRepository,
    secret_store: &dyn secret::SecretStore,
    logical: secret::LogicalCredentialId,
    previous: Option<secret::PhysicalSecretSlot>,
    value: &secret::SecretString,
) -> anyhow::Result<ResolvedSecretSlot> {
    let plan = secret::SecretBundleStagePlan::allocate(logical, previous)
        .map_err(|_| static_secret_error(ERROR_SECRET_STAGE_PLAN))?;
    repository
        .register_physical_secret_slot_staging(plan.logical_id().as_str(), plan.new_slot().as_str())
        .map_err(|_| static_secret_error(ERROR_SECRET_LEDGER_STAGE))?;

    let staged_slot = ResolvedSecretSlot {
        logical: plan.logical_id().clone(),
        physical: plan.new_slot().clone(),
    };
    if secret::stage_secret_bundle(
        secret_store,
        &plan,
        secret::SecretBundleRef::new(value, None, None),
    )
    .is_err()
    {
        cleanup_secret_slot_best_effort(repository, secret_store, &staged_slot, "stage_rollback");
        return Err(static_secret_error(ERROR_SECRET_BUNDLE_STAGE));
    }
    Ok(staged_slot)
}

fn create_dotenv_credential(
    repository: &mut dyn DotenvRepository,
    secret_store: &dyn secret::SecretStore,
    workspace_id: &str,
    key: &str,
    value: &secret::SecretString,
) -> anyhow::Result<ResolvedSecretSlot> {
    let logical = secret::LogicalCredentialId::new(uuid::Uuid::new_v4().to_string())
        .map_err(|_| static_secret_error(ERROR_SECRET_LOCATION_LOGICAL))?;
    let staged = stage_access_only_secret(repository, secret_store, logical, None, value)?;
    let draft = DotenvCredentialDraft {
        id: staged.logical.as_str().to_owned(),
        provider: "env".to_owned(),
        label: format!("{key} (.env)"),
        credential_kind: "api_key".to_owned(),
        masked_hint: Some(secret::masked_hint(value.expose())),
        workspace_id: Some(workspace_id.to_owned()),
    };
    if repository
        .insert_credential_with_secret_slot(&draft, staged.physical.as_str())
        .is_err()
    {
        cleanup_secret_slot_best_effort(repository, secret_store, &staged, "create_rollback");
        return Err(static_secret_error(ERROR_SECRET_CREATE_PUBLISH));
    }
    Ok(staged)
}

/// Rotate only when the access value actually changed. A missing/corrupt old bundle is repaired by
/// publishing a fresh access-only bundle. Once CAS publishes the new pointer, old cleanup is
/// best-effort because its durable orphan row is the crash-safe retry source.
fn rotate_dotenv_credential(
    repository: &mut dyn DotenvRepository,
    secret_store: &dyn secret::SecretStore,
    credential_id: &str,
    value: &secret::SecretString,
) -> anyhow::Result<bool> {
    let previous = resolve_secret_slot(repository, credential_id)?;
    if let Ok(bundle) = secret::read_secret_bundle(secret_store, &previous.physical)
        && bundle.access().expose() == value.expose()
        && bundle.refresh().is_none()
        && bundle.dcr().is_none()
    {
        return Ok(false);
    }

    let staged = stage_access_only_secret(
        repository,
        secret_store,
        previous.logical.clone(),
        Some(previous.physical.clone()),
        value,
    )?;
    let published = match repository.publish_credential_secret_slot_cas(
        previous.logical.as_str(),
        previous.physical.as_str(),
        staged.physical.as_str(),
        Some(&secret::masked_hint(value.expose())),
    ) {
        Ok(published) => published,
        Err(_) => {
            cleanup_secret_slot_best_effort(repository, secret_store, &staged, "rotate_rollback");
            return Err(static_secret_error(ERROR_SECRET_ROTATE_PUBLISH));
        }
    };
    if !published {
        cleanup_secret_slot_best_effort(repository, secret_store, &staged, "rotate_stale_cleanup");
        return Err(static_secret_error(ERROR_SECRET_ROTATE_STALE));
    }
    cleanup_secret_slot_best_effort(repository, secret_store, &previous, "rotate_old_cleanup");
    Ok(true)
}

fn retire_dotenv_credential(
    repository: &mut dyn DotenvRepository,
    secret_store: &dyn secret::SecretStore,
    target: &ResolvedSecretSlot,
) -> anyhow::Result<bool> {
    let deleted = repository
        .delete_credential_if_unused_cas(target.logical.as_str(), target.physical.as_str())
        .map_err(|_| static_secret_error(ERROR_SECRET_DELETE_CAS))?;
    if !deleted {
        return Ok(false);
    }
    cleanup_secret_slot(repository, secret_store, target)?;
    Ok(true)
}

struct TempFileGuard(Option<std::path::PathBuf>);

impl TempFileGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct PreparedEnvFile {
    path: std::path::PathBuf,
    temp: std::path::PathBuf,
    guard: TempFileGuard,
}

impl PreparedEnvFile {
    fn commit(mut self, expected: Option<Option<&str>>) -> anyhow::Result<()> {
        if let Some(expected) = expected {
            let mut budget = DOTENV_TOTAL_BYTES_MAX;
            let current = read_dotenv_file_app_bounded(&self.path, &mut budget)?;
            anyhow::ensure!(current.as_deref() == expected, "dotenv_source_changed");
        }
        atomic_replace(&self.temp, &self.path)
            .map_err(|_| anyhow::anyhow!("dotenv_replace_failed"))?;
        self.guard.disarm();
        #[cfg(unix)]
        std::fs::File::open(
            self.path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("dotenv_parent_missing"))?,
        )
        .and_then(|directory| directory.sync_all())
        .map_err(|_| anyhow::anyhow!("dotenv_directory_sync_failed"))?;
        Ok(())
    }
}

#[cfg(test)]
fn atomic_write_env(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    prepare_env_file(path, contents)?.commit(None)
}

fn prepare_env_file(path: &Path, contents: &[u8]) -> anyhow::Result<PreparedEnvFile> {
    anyhow::ensure!(contents.len() <= DOTENV_TOTAL_BYTES_MAX, ERROR_DOTENV_BYTES);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("dotenv_parent_missing"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(".env");
    let temp = parent.join(format!(
        ".{file_name}.deppy-tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    configure_no_follow(&mut options);
    let mut file = options
        .open(&temp)
        .map_err(|_| anyhow::anyhow!("dotenv_temp_create_failed"))?;
    let guard = TempFileGuard(Some(temp.clone()));

    // 기존 파일의 접근 권한을 유지한다. 새 파일은 OpenOptions의 0600(Unix) 기본을 쓴다.
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "dotenv_target_type_invalid"
            );
            std::fs::set_permissions(&temp, metadata.permissions())
                .map_err(|_| anyhow::anyhow!("dotenv_temp_permissions_failed"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => anyhow::bail!("dotenv_target_metadata_failed"),
    }
    file.write_all(contents)
        .map_err(|_| anyhow::anyhow!("dotenv_temp_write_failed"))?;
    file.sync_all()
        .map_err(|_| anyhow::anyhow!("dotenv_temp_sync_failed"))?;
    drop(file);

    Ok(PreparedEnvFile {
        path: path.to_owned(),
        temp,
        guard,
    })
}

#[cfg(unix)]
fn atomic_replace(temp: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(temp, target)
}

#[cfg(windows)]
fn atomic_replace(temp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 두 경로는 호출 동안 살아 있는 NUL 종료 UTF-16 버퍼다.
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace(temp: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(temp, target)
}

/// 병합 목록의 삭제는 모든 정의를 제거한다. 값 수정은 실효 값을 가진 파일을 사용한다.
pub fn write_env_var(root: &Path, key: &str, value: Option<&str>) -> anyhow::Result<()> {
    write_env_var_in_file(root, None, key, value)
}

/// 명시한 파일만 수정한다. None은 병합 목록의 수정/전체 삭제다.
pub fn write_env_var_in_file(
    root: &Path,
    target: Option<&str>,
    key: &str,
    value: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        key.len() <= DOTENV_KEY_BYTES_MAX,
        "dotenv_key_bytes_exceeded"
    );
    let valid = parse_dotenv_bounded(&format!("{key}="), 1)?;
    anyhow::ensure!(valid.len() == 1 && valid[0].0 == key, "dotenv_key_invalid");
    if let Some(value) = value {
        anyhow::ensure!(
            value.len() <= DOTENV_VALUE_BYTES_MAX,
            "dotenv_value_bytes_exceeded"
        );
        // 줄 단위 편집기가 보존할 수 없는 값을 기록해 다른 키로 해석시키지 않는다.
        anyhow::ensure!(
            !value.contains(['\n', '\r']),
            "dotenv_multiline_value_unsupported"
        );
    }
    let selected = target
        .map(|name| {
            DOTENV_FILE_NAMES
                .iter()
                .position(|file| *file == name)
                .ok_or_else(|| anyhow::anyhow!("dotenv_target_invalid"))
        })
        .transpose()?;
    let originals = read_env_file_set(root)?;
    let mut remaining = DOTENV_ENTRIES_MAX;
    let mut contains_key = Vec::new();
    for content in &originals {
        let parsed = parse_dotenv_bounded(content.as_deref().unwrap_or_default(), remaining)?;
        remaining -= parsed.len();
        contains_key.push(parsed.iter().any(|(existing, _)| existing == key));
    }
    let effective = contains_key.iter().rposition(|found| *found).unwrap_or(0);
    let mut candidates = originals.clone();
    for (index, candidate) in candidates.iter_mut().enumerate() {
        let edit = selected.map_or_else(
            || value.is_none() || index == effective,
            |selected| selected == index,
        );
        if edit && (value.is_some() || contains_key[index]) {
            *candidate = Some(edit_env_text(
                candidate.as_deref().unwrap_or_default(),
                key,
                value,
            )?);
        }
    }
    let total: usize = candidates.iter().flatten().map(String::len).sum();
    anyhow::ensure!(total <= DOTENV_TOTAL_BYTES_MAX, ERROR_DOTENV_BYTES);
    let mut remaining = DOTENV_ENTRIES_MAX;
    for content in candidates.iter().flatten() {
        remaining -= parse_dotenv_bounded(content, remaining)?.len();
    }
    commit_env_file_set(root, &originals, &candidates)?;
    if ensure_env_gitignored(root).is_err() {
        tracing::warn!(
            kind = "dotenv_file",
            phase = "gitignore",
            error_code = "dotenv_gitignore_failed"
        );
    }
    Ok(())
}

fn read_env_file_set(root: &Path) -> anyhow::Result<Vec<Option<String>>> {
    let mut budget = DOTENV_TOTAL_BYTES_MAX;
    DOTENV_FILE_NAMES
        .iter()
        .map(|name| read_dotenv_file_app_bounded(&root.join(name), &mut budget))
        .collect()
}

fn commit_env_file_set(
    root: &Path,
    originals: &[Option<String>],
    candidates: &[Option<String>],
) -> anyhow::Result<()> {
    // 모든 임시파일을 준비한 뒤 교체한다. 도중 실패를 성공으로 보고하지 않는다.
    let mut staged = Vec::new();
    for (index, (original, candidate)) in originals.iter().zip(candidates).enumerate() {
        if original != candidate {
            let content = candidate
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("dotenv_candidate_missing"))?;
            staged.push((
                index,
                prepare_env_file(&root.join(DOTENV_FILE_NAMES[index]), content.as_bytes())?,
            ));
        }
    }
    anyhow::ensure!(
        read_env_file_set(root)? == originals,
        "dotenv_source_changed"
    );
    for (index, prepared) in staged {
        prepared.commit(Some(originals[index].as_deref()))?;
    }
    // 파일 여러 개의 rename은 하나의 트랜잭션이 아니다. 마지막에도 외부 변경을 감지한다.
    anyhow::ensure!(
        read_env_file_set(root)? == candidates,
        "dotenv_source_changed"
    );
    Ok(())
}

fn edit_env_text(content: &str, key: &str, value: Option<&str>) -> anyhow::Result<String> {
    let render = |v: &str| -> anyhow::Result<String> {
        if v.contains('"') {
            anyhow::ensure!(!v.contains('\''), "dotenv_quotes_unsupported");
            return Ok(format!("{key}='{v}'"));
        }
        if v.is_empty()
            || v.chars()
                .any(|c| c.is_whitespace() || c == '#' || c == '\'')
        {
            Ok(format!("{key}=\"{v}\""))
        } else {
            Ok(format!("{key}={v}"))
        }
    };
    let matches_key = |line: &str| {
        let text = line.trim();
        let text = text.strip_prefix("export ").unwrap_or(text).trim_start();
        text.split_once('=')
            .is_some_and(|(existing, _)| existing.trim() == key)
    };
    let mut lines = collect_dotenv_lines_bounded(content)?;
    let last = lines.iter().rposition(|line| matches_key(line));
    if let Some(value) = value {
        let rendered = render(value)?;
        if let Some(last) = last {
            lines[last] = rendered;
            lines = lines
                .into_iter()
                .enumerate()
                .filter_map(|(index, line)| (index == last || !matches_key(&line)).then_some(line))
                .collect();
        } else {
            anyhow::ensure!(lines.len() < DOTENV_LINES_MAX, "dotenv_lines_exceeded");
            lines.push(rendered);
        }
    } else {
        lines.retain(|line| !matches_key(line));
    }
    let mut output = lines.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    Ok(output)
}

/// 값 없이 원본 파일의 존재 여부와 키별 출처만 전달한다.
#[derive(Default)]
pub struct DotenvSources {
    pub read_failed: bool,
    pub files: Vec<String>,
    pub keys: std::collections::BTreeMap<String, Vec<String>>,
}

pub fn load_dotenv_sources(root: &Path) -> anyhow::Result<DotenvSources> {
    let contents = read_env_file_set(root)?;
    let mut sources = DotenvSources::default();
    let mut remaining = DOTENV_ENTRIES_MAX;
    for (name, content) in DOTENV_FILE_NAMES.iter().zip(contents) {
        if let Some(content) = content {
            sources.files.push((*name).to_owned());
            let entries = parse_dotenv_bounded(&content, remaining)?;
            remaining -= entries.len();
            for (key, _) in entries {
                let files = sources.keys.entry(key).or_default();
                if !files.iter().any(|file| file == name) {
                    files.push((*name).to_owned());
                }
            }
        }
    }
    Ok(sources)
}

/// `.env`/`.env.local`이 git에 커밋되지 않게 `.gitignore`를 보장한다 (E2, 2026-07-13).
///
/// 시나리오상 에이전트가 프로젝트 안에서 `git add -A`를 자율 실행하므로, deppy가
/// 비밀값을 .env에 기록하는 순간이 유출 방지의 마지막 지점이다. git 저장소가
/// 아니면(루트에 .git 없음 — 워크트리 gitfile 포함) 아무것도 하지 않는다.
/// 이미 커버하는 패턴(.env / /.env / .env* / .env.*)이 있으면 추가하지 않는다.
fn ensure_env_gitignored(root: &Path) -> anyhow::Result<()> {
    if !root.join(".git").exists() {
        return Ok(());
    }
    let gitignore = root.join(".gitignore");
    let before = match std::fs::symlink_metadata(&gitignore) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "dotenv_gitignore_type_invalid"
            );
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => anyhow::bail!("dotenv_gitignore_metadata_failed"),
    };
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).append(true);
    configure_no_follow(&mut options);
    let mut file = options
        .open(&gitignore)
        .map_err(|_| anyhow::anyhow!("dotenv_gitignore_open_failed"))?;
    // GITIGNORE_LOCK_RETRY_BUDGET 주석 참조(flock+fork 레이스). 이 함수의 두 프로덕션
    // 호출 경로(app.rs execute_settings_job의 SettingsWorker 스레드,
    // execute_dotenv_sync_job의 "dotenv-sync-lazy" LazyDotenvWorker 스레드) 모두
    // 전용 백그라운드 스레드에서만 돌고 UI/렌더 스레드를 절대 타지 않으므로, 유계
    // 재시도로 블로킹해도 화면이 멎지 않는다. 상한을 넘기면 기존 fail-closed 오류를
    // 그대로 낸다 — 진짜 누수/경합은 여전히 잡는다.
    let lock_deadline = std::time::Instant::now() + GITIGNORE_LOCK_RETRY_BUDGET;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(_) if std::time::Instant::now() < lock_deadline => {
                std::thread::sleep(GITIGNORE_LOCK_RETRY_INTERVAL);
            }
            Err(_) => anyhow::bail!("dotenv_gitignore_lock_failed"),
        }
    }
    let opened = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("dotenv_gitignore_metadata_failed"))?;
    anyhow::ensure!(opened.is_file(), "dotenv_gitignore_type_invalid");
    #[cfg(unix)]
    if let Some(before) = before.as_ref() {
        use std::os::unix::fs::MetadataExt as _;
        anyhow::ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino(),
            "dotenv_gitignore_changed"
        );
    }
    anyhow::ensure!(
        opened.len() <= GITIGNORE_BYTES_MAX as u64,
        "dotenv_gitignore_bytes_exceeded"
    );
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0))
        .map_err(|_| anyhow::anyhow!("dotenv_gitignore_seek_failed"))?;
    let probe = GITIGNORE_BYTES_MAX + 1;
    let mut bytes = Vec::with_capacity((opened.len() as usize).min(probe));
    std::io::Read::by_ref(&mut file)
        .take(probe as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("dotenv_gitignore_read_failed"))?;
    anyhow::ensure!(
        bytes.len() <= GITIGNORE_BYTES_MAX,
        "dotenv_gitignore_bytes_exceeded"
    );
    let after_read = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("dotenv_gitignore_metadata_failed"))?;
    anyhow::ensure!(
        after_read.len() == bytes.len() as u64,
        "dotenv_gitignore_changed"
    );
    let content =
        String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("dotenv_gitignore_utf8_invalid"))?;
    let covers = |name: &str| -> bool {
        let rooted = format!("/{name}");
        content.lines().map(str::trim).any(|line| {
            line == name
                || line == rooted
                || line == ".env*"
                || (name.starts_with(".env.") && line == ".env.*")
        })
    };
    let missing: Vec<&str> = DOTENV_FILE_NAMES
        .iter()
        .copied()
        .filter(|name| !covers(name))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut block = String::new();
    if !content.is_empty() && !content.ends_with('\n') {
        block.push('\n');
    }
    block.push_str("# deppy: 환경변수 파일 — 비밀값 커밋 방지\n");
    for name in &missing {
        block.push_str(name);
        block.push('\n');
    }
    anyhow::ensure!(
        content.len().saturating_add(block.len()) <= GITIGNORE_BYTES_MAX,
        "dotenv_gitignore_bytes_exceeded"
    );
    let end = std::io::Seek::seek(&mut file, std::io::SeekFrom::End(0))
        .map_err(|_| anyhow::anyhow!("dotenv_gitignore_seek_failed"))?;
    let before_append = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("dotenv_gitignore_metadata_failed"))?;
    anyhow::ensure!(
        end == content.len() as u64 && before_append.len() == content.len() as u64,
        "dotenv_gitignore_changed"
    );
    file.write_all(block.as_bytes())
        .map_err(|_| anyhow::anyhow!("dotenv_gitignore_write_failed"))?;
    file.sync_all()
        .map_err(|_| anyhow::anyhow!("dotenv_gitignore_sync_failed"))?;
    tracing::info!(
        kind = "dotenv_gitignore",
        phase = "protect",
        added_count = missing.len(),
        "dotenv ignore protection added"
    );
    Ok(())
}

/// 레거시(비-dotenv) profile 변수를 `.env` 파일로 이전한다 (.env 일원화 — 2026-07-13 E1).
///
/// 과거 UI는 프로젝트 경로 없이도 DB 전용 profile(kind="local")에 변수를 받았고, 그
/// 변수는 셸 주입도 .env 기록도 되지 않는 유령 상태였다(binjari 사고). 경로가 지정된
/// 워크스페이스의 force 동기화 시 이 함수가 남은 레거시 변수를 .env로 옮기고 profile을
/// 정리한다 — 이후 sync가 파일 기준으로 dotenv profile을 재구성한다(.env가 진실).
///
/// best-effort: secret의 keyring resolve가 실패하면 그 키만 남기고 계속한다(값 유실
/// 방지 — 다음 force 동기화가 재시도). UI가 만든 credential은 참조만 사라지고 자격증명
/// 목록에 보존된다(remove_workspace_dotenv의 provider="env" 전용 삭제 관례와 구분).
pub fn migrate_legacy_profiles_to_dotenv(
    repository: &mut dyn DotenvRepository,
    secret_store: &dyn secret::SecretStore,
    workspace_id: &str,
    root: &Path,
) -> anyhow::Result<usize> {
    let legacy: Vec<_> = repository
        .list_env_profiles(workspace_id, DOTENV_PROFILE_SCAN_MAX)?
        .into_iter()
        .filter(|p| p.kind != DOTENV_PROFILE_KIND)
        .collect();
    if legacy.is_empty() {
        return Ok(0);
    }
    let mut migrated = 0usize;
    for profile in &legacy {
        let vars = repository.list_env_vars(&profile.id, DOTENV_VARIABLE_SCAN_MAX)?;
        let mut remaining = vars.len();
        for var in &vars {
            let value = match &var.value {
                EnvValue::Plain(v) => v.clone(),
                EnvValue::Secret { credential_id } => {
                    let slot = match resolve_secret_slot(repository, credential_id) {
                        Ok(slot) => slot,
                        Err(error) => {
                            warn_secret_failure("legacy_resolve", secret_error_code(&error));
                            continue;
                        }
                    };
                    let Ok(bundle) = secret::read_secret_bundle(secret_store, &slot.physical)
                    else {
                        warn_secret_failure("legacy_read", ERROR_SECRET_BUNDLE_READ);
                        continue;
                    };
                    bundle.access().expose().to_owned()
                }
            };
            if write_env_var(root, &var.key, Some(&value)).is_err() {
                tracing::warn!(
                    kind = "dotenv_migration",
                    phase = "write",
                    error_code = "dotenv_migration_write_failed",
                    "legacy dotenv migration failed"
                );
                continue;
            }
            repository.delete_env_var(&profile.id, &var.key)?;
            migrated += 1;
            remaining -= 1;
        }
        if remaining == 0 {
            repository.delete_env_profile(&profile.id)?;
        }
    }
    if migrated > 0 {
        tracing::info!(migrated, "레거시 env profile → .env 이전 완료");
    }
    Ok(migrated)
}

/// 프로젝트 폴더 **해제** 시 dotenv 자동 profile을 통째로 정리한다(2026-07-10):
/// 변수 → 전용 credential(참조 없을 때만) → keyring → profile 순. `.env` 파일이
/// 원본이므로 재지정 시 그대로 복구된다 — 해제했는데 키가 화면에 남는 문제 해결.
pub fn remove_workspace_dotenv(
    repository: &mut dyn DotenvRepository,
    secret_store: &dyn secret::SecretStore,
    workspace_id: &str,
) -> anyhow::Result<usize> {
    let Some(profile) = repository
        .list_env_profiles(workspace_id, DOTENV_PROFILE_SCAN_MAX)?
        .into_iter()
        .find(|p| p.kind == DOTENV_PROFILE_KIND)
    else {
        return Ok(0);
    };
    let vars = repository.list_env_vars(&profile.id, DOTENV_VARIABLE_SCAN_MAX)?;
    // dotenv가 **직접 만든** credential(provider="env")만 삭제 후보 — 사용자가 dotenv
    // profile에 수동으로 붙인 외부 credential은 참조가 사라져도 보존한다(codex High).
    let dotenv_owned = repository
        .list_dotenv_owned_credential_ids(secret::VERSIONED_SECRET_BUNDLE_SLOT_CEILING)?;
    let mut removed = 0usize;
    for var in &vars {
        let cleanup_target = if let EnvValue::Secret { credential_id } = &var.value
            && dotenv_owned.contains(credential_id)
        {
            match resolve_secret_slot(repository, credential_id) {
                Ok(slot) => Some(slot),
                Err(error) => {
                    warn_secret_failure("remove_resolve", secret_error_code(&error));
                    None
                }
            }
        } else {
            None
        };
        repository.delete_env_var(&profile.id, &var.key)?;
        if let Some(target) = cleanup_target
            && let Err(error) = retire_dotenv_credential(repository, secret_store, &target)
        {
            warn_secret_failure("remove_cleanup", secret_error_code(&error));
        }
        removed += 1;
    }
    repository.delete_env_profile(&profile.id)?;
    tracing::info!(removed, "프로젝트 해제 — dotenv profile 정리");
    Ok(removed)
}

/// 루트의 dotenv 파일들(`DOTENV_FILE_NAMES` 순서)을 읽어 병합 파싱한다.
/// 같은 키는 뒤 파일 값이 이긴다(순서는 처음 등장 위치 유지). 읽은 파일이 없으면 None.
///
/// 존재하는 파일의 권한/UTF-8/I/O 오류는 반드시 호출자에 전달한다. 한 파일의 읽기 실패를
/// "파일 없음"으로 취급하면 다른 파일의 부분 결과로 동기화가 진행되어, 읽지 못한 파일의
/// 키를 DB/keyring에서 삭제된 것으로 오판할 수 있다.
fn read_merged_dotenv(root: &Path) -> anyhow::Result<Option<Vec<(String, String)>>> {
    read_dotenv_merged_app_bounded(root)
}

/// Performs only bounded dotenv source I/O + parsing and returns a redacted plan. No DB or keyring
/// operation occurs here. A missing pair of dotenv files remains `None`, preserving the existing
/// rule that a transient checkout difference must not erase persisted environment state.
pub fn load_workspace_dotenv_plan(root: &Path) -> anyhow::Result<Option<DotenvSyncPlan>> {
    let Some(entries) = read_merged_dotenv(root)? else {
        return Ok(None);
    };
    let value_bytes = entries.iter().try_fold(0usize, |total, (_, value)| {
        total
            .checked_add(value.len())
            .ok_or_else(|| static_secret_error(ERROR_DOTENV_BYTES))
    })?;
    anyhow::ensure!(
        entries.len() <= DOTENV_ENTRIES_MAX && value_bytes <= DOTENV_TOTAL_BYTES_MAX,
        ERROR_DOTENV_BYTES
    );
    Ok(Some(DotenvSyncPlan {
        entries,
        value_bytes,
    }))
}

/// Applies one already bounded dotenv plan to persistence/keyring state. This phase performs no
/// filesystem reads or writes, which gives the app worker an exact failure-injection seam and
/// prevents a source re-read between validation and application.
pub fn apply_workspace_dotenv_plan(
    repository: &mut dyn DotenvRepository,
    secret_store: &dyn secret::SecretStore,
    redaction: &secret::RedactionService,
    workspace_id: &str,
    plan: DotenvSyncPlan,
) -> anyhow::Result<DotenvSyncReport> {
    enum PreparedDotenvValue {
        Plain(String),
        Secret {
            value: secret::SecretString,
            _redaction_lease: secret::RedactionLease,
        },
    }

    // Secure the complete bounded secret set before the first persistence/keyring mutation. If
    // even one value cannot enter the rotating redaction corpus, every previously acquired lease
    // drops here and the dotenv profile remains untouched.
    let mut prepared = Vec::with_capacity(plan.entries.len());
    let mut skipped_keys: Vec<String> = Vec::new();
    for (key, value) in plan.entries {
        // Secret classification must match storage validation: a value the repository refuses to
        // persist as plain is prepared through the same redacted physical-slot path.
        // 마스킹할 수 없는 값은 비밀로 다룰 수 없다(2026-08-21).
        //
        // `is_secret_key`는 부분 문자열 판정이라 매우 넓다 — "OAUTH"가 AUTH를,
        // "ALLOW_PRIVATE_URLS"가 PRIVATE를 물어 secret으로 잡힌다. 그런데 그 값이
        // redaction 최소 길이(6바이트) 미만이면 코퍼스에 등록할 수 없고, 예전엔 그것이
        // 동기화 전체의 실패가 되어 `.env`의 빈 플레이스홀더 한 줄이 워크스페이스를
        // 통째로 막았다(사용자 보고: `GITHUB_OAUTH_CLIENT_ID=`).
        //
        // 빈 값은 지킬 내용이 아예 없고, 1~5바이트 값도 마스킹이 불가능하다. 그런 값은
        // 애초에 비밀이 아니므로 plain으로 저장한다 — 런타임의 fail-closed 계약
        // (`resolve_secret_set`)은 건드리지 않는다. 저장소가 plain을 거부하면 예전대로
        // 비밀 경로로 보내 fail-closed를 유지한다.
        // 빈 값은 자격증명이 아니다 — 지킬 내용이 아예 없다. 키 이름만 보고 비밀로
        // 분류하면 `.env`의 빈 플레이스홀더 한 줄이 워크스페이스를 통째로 막는다
        // (2026-08-21 사용자 보고: `GITHUB_OAUTH_CLIENT_ID=`가 "OAUTH" 안의 AUTH에
        // 걸려 비밀이 되고, 빈 값은 redaction 최소 길이에 미달해 동기화 전체가 죽었다).
        //
        // 빈 값이 **아닌** 짧은 값에는 이 예외를 주지 않는다. `is_secret_key`(부분문자열,
        // 넓음)와 저장소의 `secret_like_env_key`(정확/접미사, 좁음) 사이에는 간극이 있어
        // (`DB_PWD`, `*_CREDENTIAL` 등은 넓은 쪽만 잡는다), 길이만 보고 넓은 판정을
        // 건너뛰면 `DB_PWD=1234` 같은 **진짜** 짧은 비밀이 SQLite에 평문으로 저장되고
        // redaction 등록도 되지 않아 로그에 그대로 남는다(2026-08-21 리뷰 HIGH).
        let needs_secret = if value.is_empty() {
            !repository.plain_env_value_allowed(&key, &value)
        } else {
            is_secret_key(&key) || !repository.plain_env_value_allowed(&key, &value)
        };
        let value = if needs_secret {
            let value = secret::SecretString::new(value);
            match redaction.acquire_rotating(&value) {
                Ok(redaction_lease) => PreparedDotenvValue::Secret {
                    value,
                    _redaction_lease: redaction_lease,
                },
                // **이 값 하나**가 너무 짧아 마스킹할 수 없는 경우만 그 항목을 뺀다
                // (2026-08-21). 예전엔 동기화 전체를 죽였고, 그래서 `.env` 50줄 중
                // 1줄이 걸리면 나머지 49줄도 반영되지 않고 워크스페이스가 통째로 막혔다.
                //
                // 빼는 것이 평문 저장보다 안전하다 — 마스킹할 수 없는 값을 SQLite에
                // 평문으로 남기면 로그로도 샌다. 제외한 키는 리포트로 올린다.
                // `SecretTooLarge`는 여기에 **넣지 않는다**. 리뷰에서 "값 하나의 길이만
                // 보는 값 단위 실패"라는 지적이 있었으나 사실이 아니다 — 그 상한은
                // `maximum_input_len() = max_bytes / 128`로 **코퍼스 설정에서 파생**된다.
                // 코퍼스가 작게 잡히면 모든 값이 too-large가 되고, 전부 제외되면 뒤의
                // prune 루프가 기존 credential과 키체인 항목을 통째로 지운다. 기존 계약
                // 테스트 2건이 정확히 그 시나리오를 지킨다(2026-08-21, 되돌린 시도).
                Err(secret::RedactionCapacityError::SecretTooShort { .. }) => {
                    tracing::warn!(
                        kind = "dotenv",
                        phase = "prepare",
                        error_code = "unprotectable_value",
                        key = %key,
                        "dotenv value is too short to mask; excluded from this workspace"
                    );
                    skipped_keys.push(key);
                    continue;
                }
                // 코퍼스 고갈·fail-closed 같은 **계통** 실패는 예전대로 전체를 거부한다.
                // 이걸 항목별 제외로 처리하면 모든 비밀이 빠지고, 뒤의 prune 루프가
                // 기존 credential과 키체인 항목을 통째로 지운다 — 일시적 장애가 영구
                // 데이터 손실이 된다(2026-08-21, 기존 계약 테스트 2건이 이걸 지킨다).
                Err(_) => return Err(static_secret_error(ERROR_SECRET_REDACTION)),
            }
        } else {
            PreparedDotenvValue::Plain(value)
        };
        prepared.push((key, value));
    }

    // dotenv profile 찾기/생성.
    let profile_id = match repository
        .list_env_profiles(workspace_id, DOTENV_PROFILE_SCAN_MAX)?
        .into_iter()
        .find(|p| p.kind == DOTENV_PROFILE_KIND)
    {
        Some(p) => p.id,
        None => {
            repository.insert_env_profile(workspace_id, DOTENV_PROFILE_NAME, DOTENV_PROFILE_KIND)?
        }
    };

    let existing = repository.list_env_vars(&profile_id, DOTENV_VARIABLE_SCAN_MAX)?;
    let dotenv_owned = repository
        .list_dotenv_owned_credential_ids(secret::VERSIONED_SECRET_BUNDLE_SLOT_CEILING)?;
    let mut report = DotenvSyncReport::default();
    skipped_keys.truncate(DOTENV_SKIPPED_KEYS_MAX);
    report.skipped_keys = skipped_keys;

    for (key, value) in &prepared {
        let current = existing.iter().find(|v| &v.key == key);
        if let PreparedDotenvValue::Secret { value: secret, .. } = value {
            let (credential_id, newly_created) = match current.map(|v| &v.value) {
                Some(EnvValue::Secret { credential_id })
                    if dotenv_owned.contains(credential_id) =>
                {
                    match rotate_dotenv_credential(repository, secret_store, credential_id, secret)
                    {
                        Ok(false) => continue,
                        Ok(true) => (credential_id.clone(), None),
                        Err(error) => {
                            let error_code = secret_error_code(&error);
                            warn_secret_failure("rotate", error_code);
                            return Err(static_secret_error(error_code));
                        }
                    }
                }
                _ => match create_dotenv_credential(
                    repository,
                    secret_store,
                    workspace_id,
                    key,
                    secret,
                ) {
                    Ok(created) => (created.logical.as_str().to_owned(), Some(created)),
                    Err(error) => {
                        let error_code = secret_error_code(&error);
                        warn_secret_failure("create", error_code);
                        return Err(static_secret_error(error_code));
                    }
                },
            };
            if repository
                .upsert_env_var(
                    &profile_id,
                    key,
                    &EnvValue::Secret {
                        credential_id: credential_id.clone(),
                    },
                )
                .is_err()
            {
                if let Some(created) = newly_created.as_ref()
                    && let Err(error) = retire_dotenv_credential(repository, secret_store, created)
                {
                    warn_secret_failure("bind_rollback", secret_error_code(&error));
                }
                return Err(static_secret_error(ERROR_SECRET_ENV_BIND));
            }
            report.upserted += 1;
        } else if let PreparedDotenvValue::Plain(value) = value {
            // plain: 값이 같으면 write 생략 (DB churn 방지).
            if matches!(current.map(|v| &v.value), Some(EnvValue::Plain(v)) if v == value) {
                continue;
            }
            let replaced_secret = match current.map(|v| &v.value) {
                Some(EnvValue::Secret { credential_id })
                    if dotenv_owned.contains(credential_id) =>
                {
                    match resolve_secret_slot(repository, credential_id) {
                        Ok(slot) => Some(slot),
                        Err(error) => {
                            let error_code = secret_error_code(&error);
                            warn_secret_failure("plain_replace_resolve", error_code);
                            return Err(static_secret_error(error_code));
                        }
                    }
                }
                _ => None,
            };
            repository.upsert_env_var(&profile_id, key, &EnvValue::Plain(value.clone()))?;
            if let Some(target) = replaced_secret
                && let Err(error) = retire_dotenv_credential(repository, secret_store, &target)
            {
                warn_secret_failure("plain_replace_cleanup", secret_error_code(&error));
            }
            report.upserted += 1;
        }
    }

    // 병합 결과에서 사라진 키 제거 (+ 이 profile 전용 credential 정리).
    for var in &existing {
        if prepared.iter().any(|(k, _)| k == &var.key) {
            continue;
        }
        let cleanup_target = if let EnvValue::Secret { credential_id } = &var.value
            && dotenv_owned.contains(credential_id)
        {
            match resolve_secret_slot(repository, credential_id) {
                Ok(slot) => Some(slot),
                Err(error) => {
                    let error_code = secret_error_code(&error);
                    warn_secret_failure("prune_resolve", error_code);
                    return Err(static_secret_error(error_code));
                }
            }
        } else {
            None
        };
        repository.delete_env_var(&profile_id, &var.key)?;
        if let Some(target) = cleanup_target
            && let Err(error) = retire_dotenv_credential(repository, secret_store, &target)
        {
            warn_secret_failure("prune_cleanup", secret_error_code(&error));
        }
        report.removed += 1;
    }
    Ok(report)
}

/// Default inactivity window after which the dotenv worker drops its resources and exits.
///
/// The timeout is observed only while the bounded request queues are empty. It is a lifecycle
/// deadline, not a polling or retry interval.
pub const DOTENV_WORKER_IDLE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
/// Exact protocol continuations retained by one dotenv worker.
pub const DOTENV_WORKER_CONTINUATION_MAX: usize = 8;

/// Correlates a dotenv operation without retaining workspace, path, or secret-bearing data.
///
/// Generation and revision are checked together before an outcome may be applied. Operation IDs
/// are returned separately for trace correlation. Debug output deliberately hides all three
/// high-cardinality values.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct DotenvWorkerCorrelation {
    generation: u64,
    revision: u64,
    operation_id: u64,
}

impl DotenvWorkerCorrelation {
    pub fn new(generation: u64, revision: u64, operation_id: u64) -> Self {
        Self {
            generation,
            revision,
            operation_id,
        }
    }

    pub fn generation(self) -> u64 {
        self.generation
    }

    pub fn revision(self) -> u64 {
        self.revision
    }

    pub fn operation_id(self) -> u64 {
        self.operation_id
    }

    pub fn is_current(self, generation: u64, revision: u64) -> bool {
        self.generation == generation && self.revision == revision
    }
}

impl std::fmt::Debug for DotenvWorkerCorrelation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DotenvWorkerCorrelation")
            .field("correlation", &"REDACTED")
            .finish()
    }
}

/// Static, low-cardinality failure codes crossing the worker boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DotenvWorkerErrorCode {
    #[cfg(test)]
    ResourceOpenFailed,
    ExecuteFailed,
    WorkerPanicked,
    ThreadSpawnFailed,
    ContinuationLimit,
    DuplicateOperation,
    StaleOutcome,
}

impl std::fmt::Display for DotenvWorkerErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            #[cfg(test)]
            Self::ResourceOpenFailed => "dotenv_worker_resource_open_failed",
            Self::ExecuteFailed => "dotenv_worker_execute_failed",
            Self::WorkerPanicked => "dotenv_worker_panicked",
            Self::ThreadSpawnFailed => "dotenv_worker_thread_spawn_failed",
            Self::ContinuationLimit => "dotenv_worker_continuation_limit",
            Self::DuplicateOperation => "dotenv_worker_duplicate_operation",
            Self::StaleOutcome => "dotenv_worker_stale_outcome",
        })
    }
}

impl std::error::Error for DotenvWorkerErrorCode {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DotenvWorkerRequestKind {
    State,
    Continuation,
}

struct DotenvWorkerEnvelope<J> {
    sequence: u64,
    kind: DotenvWorkerRequestKind,
    correlation: DotenvWorkerCorrelation,
    payload: J,
}

/// A state request displaced before execution by a newer state request.
///
/// Returning ownership lets callers explicitly discard or reconcile a secret-bearing payload;
/// Debug never formats that payload or its correlation.
pub struct ReplacedDotenvState<J> {
    correlation: DotenvWorkerCorrelation,
    payload: J,
}

impl<J> ReplacedDotenvState<J> {
    #[cfg(test)]
    pub fn correlation(&self) -> DotenvWorkerCorrelation {
        self.correlation
    }

    #[cfg(test)]
    pub fn into_payload(self) -> J {
        self.payload
    }

    pub fn into_parts(self) -> (DotenvWorkerCorrelation, J) {
        (self.correlation, self.payload)
    }
}

impl<J> std::fmt::Debug for ReplacedDotenvState<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReplacedDotenvState")
            .field("payload", &"REDACTED")
            .finish()
    }
}

/// Submission failure that returns ownership of the rejected payload.
pub struct DotenvWorkerSubmitError<J> {
    code: DotenvWorkerErrorCode,
    correlation: DotenvWorkerCorrelation,
    payload: J,
}

impl<J> DotenvWorkerSubmitError<J> {
    #[cfg(test)]
    pub fn code(&self) -> DotenvWorkerErrorCode {
        self.code
    }

    #[cfg(test)]
    pub fn correlation(&self) -> DotenvWorkerCorrelation {
        self.correlation
    }

    #[cfg(test)]
    pub fn into_payload(self) -> J {
        self.payload
    }

    pub fn into_parts(self) -> (DotenvWorkerErrorCode, DotenvWorkerCorrelation, J) {
        (self.code, self.correlation, self.payload)
    }
}

impl<J> std::fmt::Debug for DotenvWorkerSubmitError<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DotenvWorkerSubmitError")
            .field("code", &self.code)
            .field("payload", &"REDACTED")
            .finish()
    }
}

/// One bounded worker outcome. Payload Debug and correlation values never cross diagnostics.
pub struct DotenvWorkerOutcome<O> {
    kind: DotenvWorkerRequestKind,
    correlation: DotenvWorkerCorrelation,
    result: Result<O, DotenvWorkerErrorCode>,
}

impl<O> DotenvWorkerOutcome<O> {
    pub fn operation_id(&self) -> u64 {
        self.correlation.operation_id()
    }

    pub fn is_continuation(&self) -> bool {
        self.kind == DotenvWorkerRequestKind::Continuation
    }

    /// Rejects stale generation/revision results before exposing their payload to application
    /// code. The rejected outcome is dropped in this method.
    pub fn into_current(
        self,
        generation: u64,
        revision: u64,
    ) -> Result<Self, DotenvWorkerErrorCode> {
        if self.correlation.is_current(generation, revision) {
            Ok(self)
        } else {
            Err(DotenvWorkerErrorCode::StaleOutcome)
        }
    }

    pub fn into_result(self) -> Result<O, DotenvWorkerErrorCode> {
        self.result
    }
}

impl<O> std::fmt::Debug for DotenvWorkerOutcome<O> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DotenvWorkerOutcome")
            .field("kind", &self.kind)
            .field("result", &self.result.as_ref().err())
            .field("payload", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DotenvWorkerLifecycle {
    Running,
    Stopping,
    Exited,
}

struct DotenvWorkerThreadState<J> {
    lifecycle: DotenvWorkerLifecycle,
    state: Option<DotenvWorkerEnvelope<J>>,
    continuations: std::collections::VecDeque<DotenvWorkerEnvelope<J>>,
}

impl<J> DotenvWorkerThreadState<J> {
    fn new() -> Self {
        Self {
            lifecycle: DotenvWorkerLifecycle::Running,
            state: None,
            continuations: std::collections::VecDeque::with_capacity(
                DOTENV_WORKER_CONTINUATION_MAX,
            ),
        }
    }

    fn has_pending(&self) -> bool {
        self.state.is_some() || !self.continuations.is_empty()
    }

    fn take_next(&mut self) -> Option<DotenvWorkerEnvelope<J>> {
        match (self.state.as_ref(), self.continuations.front()) {
            (Some(state), Some(continuation)) if state.sequence < continuation.sequence => {
                self.state.take()
            }
            (Some(_), Some(_)) | (None, Some(_)) => self.continuations.pop_front(),
            (Some(_), None) => self.state.take(),
            (None, None) => None,
        }
    }
}

struct DotenvWorkerExitGuard<J> {
    state: std::sync::Arc<std::sync::Mutex<DotenvWorkerThreadState<J>>>,
}

impl<J> Drop for DotenvWorkerExitGuard<J> {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .lifecycle = DotenvWorkerLifecycle::Exited;
    }
}

struct DotenvWorkerSlot<J, O> {
    wake_tx: Option<std::sync::mpsc::SyncSender<()>>,
    result_rx: Option<std::sync::mpsc::Receiver<DotenvWorkerOutcome<O>>>,
    handle: Option<std::thread::JoinHandle<()>>,
    state: std::sync::Arc<std::sync::Mutex<DotenvWorkerThreadState<J>>>,
}

type DotenvResourceFactory<R> =
    dyn Fn() -> Result<R, DotenvWorkerErrorCode> + Send + Sync + 'static;
type DotenvJobExecutor<J, O, R> =
    dyn Fn(&mut R, J) -> Result<O, DotenvWorkerErrorCode> + Send + Sync + 'static;
type DotenvCompletionWake = dyn Fn() + Send + Sync + 'static;

/// Lazy, bounded, app-independent execution primitive for dotenv freshness work.
///
/// Construction only stores closure ports: it creates no thread, channel, persistence handle,
/// keyring handle, timer, or repaint. The first accepted request spawns one standard thread. That
/// thread opens `R` only after taking its first request, reuses it while active, and drops it on a
/// lifecycle-locked idle exit. There is one latest-only pending state slot, eight exact FIFO
/// continuations, a one-item wake channel, and a one-item result channel. The caller-provided wake
/// callback runs once only after an outcome enters the result channel; construction and idle exit
/// never invoke it.
pub struct LazyDotenvWorker<J: Send + 'static, O: Send + 'static, R: Send + 'static> {
    idle_ttl: std::time::Duration,
    factory: std::sync::Arc<DotenvResourceFactory<R>>,
    execute: std::sync::Arc<DotenvJobExecutor<J, O, R>>,
    wake: std::sync::Arc<DotenvCompletionWake>,
    slot: Option<DotenvWorkerSlot<J, O>>,
    retired: std::collections::VecDeque<DotenvWorkerOutcome<O>>,
    next_sequence: u64,
    continuation_operations: std::collections::HashSet<u64>,
}

impl<J: Send + 'static, O: Send + 'static, R: Send + 'static> LazyDotenvWorker<J, O, R> {
    pub fn new(
        factory: impl Fn() -> Result<R, DotenvWorkerErrorCode> + Send + Sync + 'static,
        execute: impl Fn(&mut R, J) -> Result<O, DotenvWorkerErrorCode> + Send + Sync + 'static,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self::with_idle_ttl(DOTENV_WORKER_IDLE_TTL, factory, execute, wake)
    }

    /// Constructor seam for deterministic lifecycle tests. Production callers use [`Self::new`].
    pub fn with_idle_ttl(
        idle_ttl: std::time::Duration,
        factory: impl Fn() -> Result<R, DotenvWorkerErrorCode> + Send + Sync + 'static,
        execute: impl Fn(&mut R, J) -> Result<O, DotenvWorkerErrorCode> + Send + Sync + 'static,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            idle_ttl,
            factory: std::sync::Arc::new(factory),
            execute: std::sync::Arc::new(execute),
            wake: std::sync::Arc::new(wake),
            slot: None,
            retired: std::collections::VecDeque::with_capacity(DOTENV_WORKER_CONTINUATION_MAX + 1),
            next_sequence: 0,
            continuation_operations: std::collections::HashSet::with_capacity(
                DOTENV_WORKER_CONTINUATION_MAX,
            ),
        }
    }

    /// Replaces only a not-yet-started state request. An executing request is never cancelled or
    /// retried. The displaced payload is returned to the caller.
    pub fn request_state(
        &mut self,
        correlation: DotenvWorkerCorrelation,
        payload: J,
    ) -> Result<Option<ReplacedDotenvState<J>>, DotenvWorkerSubmitError<J>> {
        let envelope = self.envelope(DotenvWorkerRequestKind::State, correlation, payload);
        self.submit(envelope).map(|replaced| {
            replaced.map(|replaced| ReplacedDotenvState {
                correlation: replaced.correlation,
                payload: replaced.payload,
            })
        })
    }

    /// Enqueues one exact continuation or returns its payload when the eight-operation aggregate
    /// admission limit is full. Accepted continuations are never replaced by later requests.
    pub fn request_continuation(
        &mut self,
        correlation: DotenvWorkerCorrelation,
        payload: J,
    ) -> Result<(), DotenvWorkerSubmitError<J>> {
        if self
            .continuation_operations
            .contains(&correlation.operation_id())
        {
            return Err(DotenvWorkerSubmitError {
                code: DotenvWorkerErrorCode::DuplicateOperation,
                correlation,
                payload,
            });
        }
        if self.continuation_operations.len() >= DOTENV_WORKER_CONTINUATION_MAX {
            return Err(DotenvWorkerSubmitError {
                code: DotenvWorkerErrorCode::ContinuationLimit,
                correlation,
                payload,
            });
        }
        let envelope = self.envelope(DotenvWorkerRequestKind::Continuation, correlation, payload);
        self.submit(envelope)?;
        self.continuation_operations
            .insert(correlation.operation_id());
        Ok(())
    }

    /// Returns at most one result and never waits. Retired idle-thread results are drained before
    /// results from a restarted thread.
    pub fn try_recv(&mut self) -> Option<DotenvWorkerOutcome<O>> {
        if let Some(outcome) = self.retired.pop_front() {
            self.complete_outstanding(&outcome);
            return Some(outcome);
        }

        let received = self.slot.as_ref().and_then(|slot| {
            slot.result_rx
                .as_ref()
                .and_then(|results| results.try_recv().ok())
        });
        if let Some(outcome) = received {
            self.complete_outstanding(&outcome);
            return Some(outcome);
        }

        if self.slot_exited() {
            self.retire_exited_slot();
            if let Some(outcome) = self.retired.pop_front() {
                self.complete_outstanding(&outcome);
                return Some(outcome);
            }
        }
        None
    }

    #[cfg(test)]
    pub fn is_thread_running(&self) -> bool {
        self.slot.as_ref().is_some_and(|slot| {
            slot.state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .lifecycle
                == DotenvWorkerLifecycle::Running
        })
    }

    #[cfg(test)]
    pub fn continuation_outstanding(&self) -> usize {
        self.continuation_operations.len()
    }

    fn envelope(
        &mut self,
        kind: DotenvWorkerRequestKind,
        correlation: DotenvWorkerCorrelation,
        payload: J,
    ) -> DotenvWorkerEnvelope<J> {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        DotenvWorkerEnvelope {
            sequence,
            kind,
            correlation,
            payload,
        }
    }

    fn submit(
        &mut self,
        mut envelope: DotenvWorkerEnvelope<J>,
    ) -> Result<Option<DotenvWorkerEnvelope<J>>, DotenvWorkerSubmitError<J>> {
        loop {
            if self.slot.is_none()
                && let Err(code) = self.spawn_slot()
            {
                return Err(DotenvWorkerSubmitError {
                    code,
                    correlation: envelope.correlation,
                    payload: envelope.payload,
                });
            }

            let Some(slot) = self.slot.as_ref() else {
                unreachable!("spawn_slot installs a slot on success")
            };
            let mut state = slot
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if state.lifecycle != DotenvWorkerLifecycle::Running {
                drop(state);
                self.retire_exited_slot();
                continue;
            }

            let submitted_kind = envelope.kind;
            let replaced = match submitted_kind {
                DotenvWorkerRequestKind::State => state.state.replace(envelope),
                DotenvWorkerRequestKind::Continuation => {
                    state.continuations.push_back(envelope);
                    None
                }
            };
            match slot
                .wake_tx
                .as_ref()
                .expect("running slot wake sender")
                .try_send(())
            {
                Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => return Ok(replaced),
                Err(std::sync::mpsc::TrySendError::Disconnected(())) => {
                    state.lifecycle = DotenvWorkerLifecycle::Exited;
                    envelope = match submitted_kind {
                        DotenvWorkerRequestKind::State => {
                            let submitted = state.state.take().expect("submitted state");
                            state.state = replaced;
                            submitted
                        }
                        DotenvWorkerRequestKind::Continuation => state
                            .continuations
                            .pop_back()
                            .expect("submitted continuation"),
                    };
                    drop(state);
                    self.retire_exited_slot();
                }
            }
        }
    }

    fn spawn_slot(&mut self) -> Result<(), DotenvWorkerErrorCode> {
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<DotenvWorkerOutcome<O>>(1);
        let state = std::sync::Arc::new(std::sync::Mutex::new(DotenvWorkerThreadState::new()));
        let thread_state = std::sync::Arc::clone(&state);
        let factory = std::sync::Arc::clone(&self.factory);
        let execute = std::sync::Arc::clone(&self.execute);
        let wake = std::sync::Arc::clone(&self.wake);
        let idle_ttl = self.idle_ttl;
        let handle = std::thread::Builder::new()
            .name("dotenv-sync-lazy".to_owned())
            .spawn(move || {
                run_lazy_dotenv_worker(
                    thread_state,
                    wake_rx,
                    result_tx,
                    idle_ttl,
                    factory,
                    execute,
                    wake,
                );
            })
            .map_err(|_| DotenvWorkerErrorCode::ThreadSpawnFailed)?;
        self.slot = Some(DotenvWorkerSlot {
            wake_tx: Some(wake_tx),
            result_rx: Some(result_rx),
            handle: Some(handle),
            state,
        });
        Ok(())
    }

    fn slot_exited(&self) -> bool {
        self.slot.as_ref().is_some_and(|slot| {
            slot.state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .lifecycle
                == DotenvWorkerLifecycle::Exited
        })
    }

    fn retire_exited_slot(&mut self) {
        let Some(mut slot) = self.slot.take() else {
            return;
        };
        let lifecycle = slot
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .lifecycle;
        if lifecycle == DotenvWorkerLifecycle::Running {
            self.slot = Some(slot);
            return;
        }
        slot.wake_tx.take();
        if let Some(handle) = slot.handle.take() {
            let _ = handle.join();
        }
        if let Some(results) = slot.result_rx.take() {
            while let Ok(outcome) = results.try_recv() {
                self.retain_retired(outcome);
            }
        }
    }

    fn retain_retired(&mut self, outcome: DotenvWorkerOutcome<O>) {
        if outcome.kind == DotenvWorkerRequestKind::State
            && let Some(index) = self
                .retired
                .iter()
                .position(|existing| existing.kind == DotenvWorkerRequestKind::State)
        {
            self.retired.remove(index);
        }
        self.retired.push_back(outcome);
    }

    fn complete_outstanding(&mut self, outcome: &DotenvWorkerOutcome<O>) {
        if outcome.kind == DotenvWorkerRequestKind::Continuation {
            self.continuation_operations
                .remove(&outcome.correlation.operation_id());
        }
    }
}

impl<J: Send + 'static, O: Send + 'static, R: Send + 'static> Drop for LazyDotenvWorker<J, O, R> {
    fn drop(&mut self) {
        let Some(mut slot) = self.slot.take() else {
            return;
        };
        {
            let mut state = slot
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state.lifecycle = DotenvWorkerLifecycle::Stopping;
            state.state = None;
            state.continuations.clear();
        }
        // Close both sides before joining. In particular, dropping the result receiver releases a
        // worker blocked on the one-item result bound.
        slot.wake_tx.take();
        slot.result_rx.take();
        if let Some(handle) = slot.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_lazy_dotenv_worker<J: Send + 'static, O: Send + 'static, R: Send + 'static>(
    state: std::sync::Arc<std::sync::Mutex<DotenvWorkerThreadState<J>>>,
    wake_rx: std::sync::mpsc::Receiver<()>,
    result_tx: std::sync::mpsc::SyncSender<DotenvWorkerOutcome<O>>,
    idle_ttl: std::time::Duration,
    factory: std::sync::Arc<DotenvResourceFactory<R>>,
    execute: std::sync::Arc<DotenvJobExecutor<J, O, R>>,
    wake: std::sync::Arc<DotenvCompletionWake>,
) {
    let _exit = DotenvWorkerExitGuard {
        state: std::sync::Arc::clone(&state),
    };
    let mut resource = None;
    loop {
        let next = {
            let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
            if state.lifecycle != DotenvWorkerLifecycle::Running {
                return;
            }
            state.take_next()
        };
        if let Some(request) = next {
            let result = if resource.is_none() {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| factory())) {
                    Ok(Ok(opened)) => {
                        resource = Some(opened);
                        None
                    }
                    Ok(Err(code)) => Some(Err(code)),
                    Err(_) => Some(Err(DotenvWorkerErrorCode::WorkerPanicked)),
                }
            } else {
                None
            };
            let result = result.unwrap_or_else(|| {
                let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute(
                        resource.as_mut().expect("resource initialized"),
                        request.payload,
                    )
                }));
                match execution {
                    Ok(result) => result,
                    Err(_) => {
                        resource.take();
                        Err(DotenvWorkerErrorCode::WorkerPanicked)
                    }
                }
            });
            if result_tx
                .send(DotenvWorkerOutcome {
                    kind: request.kind,
                    correlation: request.correlation,
                    result,
                })
                .is_err()
            {
                return;
            }
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wake()));
            continue;
        }

        match wake_rx.recv_timeout(idle_ttl) {
            Ok(()) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // The final non-blocking receive and the lifecycle transition share the exact
                // mutex used by submission. A sender either queues before this check and is seen,
                // or observes Exited and restarts a new worker; no accepted request is stranded.
                let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());
                match wake_rx.try_recv() {
                    Ok(()) => continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    Err(std::sync::mpsc::TryRecvError::Empty) if state.has_pending() => continue,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        state.lifecycle = DotenvWorkerLifecycle::Exited;
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage::Db;

    fn recv_worker_outcome<J: Send + 'static, O: Send + 'static, R: Send + 'static>(
        worker: &mut LazyDotenvWorker<J, O, R>,
    ) -> DotenvWorkerOutcome<O> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(outcome) = worker.try_recv() {
                return outcome;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "dotenv worker outcome timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "dotenv worker condition timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn lazy_worker_생성은_thread_factory_execute를_시작하지_않는다() {
        let factory_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let execute_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_counter = std::sync::Arc::clone(&factory_calls);
        let execute_counter = std::sync::Arc::clone(&execute_calls);
        let wake_counter = std::sync::Arc::clone(&wake_calls);
        let worker = LazyDotenvWorker::<u64, u64, ()>::new(
            move || {
                factory_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            move |_: &mut (), value| {
                execute_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(value)
            },
            move || {
                wake_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
        );

        assert!(!worker.is_thread_running());
        assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(execute_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(wake_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn lazy_worker_첫_request만_thread와_resource를_연다() {
        let factory_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let execute_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_counter = std::sync::Arc::clone(&factory_calls);
        let execute_counter = std::sync::Arc::clone(&execute_calls);
        let wake_counter = std::sync::Arc::clone(&wake_calls);
        let mut worker = LazyDotenvWorker::new(
            move || {
                factory_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            move |_: &mut (), value: u64| {
                execute_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(value + 1)
            },
            move || {
                wake_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
        );

        assert!(
            worker
                .request_state(DotenvWorkerCorrelation::new(4, 7, 11), 40)
                .unwrap()
                .is_none()
        );
        let outcome = recv_worker_outcome(&mut worker).into_current(4, 7).unwrap();
        assert_eq!(outcome.operation_id(), 11);
        assert_eq!(outcome.into_result(), Ok(41));
        assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(execute_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(wake_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn lazy_worker_state_backlog는_최신_한건으로_교체된다() {
        let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let execute_gate = std::sync::Arc::clone(&gate);
        let execute_started = std::sync::Arc::clone(&started);
        let mut worker = LazyDotenvWorker::new(
            || Ok(()),
            move |_: &mut (), value: u64| {
                if value == 1 {
                    execute_started.store(true, std::sync::atomic::Ordering::Release);
                    let (lock, ready) = &*execute_gate;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                }
                Ok(value)
            },
            || {},
        );

        worker
            .request_state(DotenvWorkerCorrelation::new(1, 1, 1), 1)
            .unwrap();
        wait_until(|| started.load(std::sync::atomic::Ordering::Acquire));
        assert!(
            worker
                .request_state(DotenvWorkerCorrelation::new(1, 2, 2), 2)
                .unwrap()
                .is_none()
        );
        let replaced = worker
            .request_state(DotenvWorkerCorrelation::new(1, 3, 3), 3)
            .unwrap()
            .expect("second pending state is replaced");
        assert_eq!(replaced.correlation().operation_id(), 2);
        assert_eq!(replaced.into_payload(), 2);
        {
            let (lock, ready) = &*gate;
            *lock.lock().unwrap() = true;
            ready.notify_all();
        }

        let first = recv_worker_outcome(&mut worker);
        let latest = recv_worker_outcome(&mut worker);
        assert_eq!(first.operation_id(), 1);
        assert_eq!(first.into_result(), Ok(1));
        assert_eq!(latest.operation_id(), 3);
        assert_eq!(latest.into_result(), Ok(3));
        assert!(worker.try_recv().is_none());
    }

    #[test]
    fn lazy_worker_continuation은_정확히_여덟건까지만_fifo로_보존한다() {
        let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let execute_gate = std::sync::Arc::clone(&gate);
        let execute_started = std::sync::Arc::clone(&started);
        let mut worker = LazyDotenvWorker::new(
            || Ok(()),
            move |_: &mut (), value: u64| {
                if value == 0 {
                    execute_started.store(true, std::sync::atomic::Ordering::Release);
                    let (lock, ready) = &*execute_gate;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                }
                Ok(value)
            },
            || {},
        );

        worker
            .request_continuation(DotenvWorkerCorrelation::new(1, 1, 1), 0)
            .unwrap();
        wait_until(|| started.load(std::sync::atomic::Ordering::Acquire));
        let duplicate = worker
            .request_continuation(DotenvWorkerCorrelation::new(u64::MAX, u64::MAX, 1), 9_999)
            .unwrap_err();
        assert_eq!(duplicate.code(), DotenvWorkerErrorCode::DuplicateOperation);
        assert_eq!(duplicate.into_payload(), 9_999);
        for value in 1..DOTENV_WORKER_CONTINUATION_MAX as u64 {
            worker
                .request_continuation(DotenvWorkerCorrelation::new(1, 1, value + 1), value)
                .unwrap();
        }
        let rejected = worker
            .request_continuation(DotenvWorkerCorrelation::new(1, 1, 99), 99)
            .unwrap_err();
        assert_eq!(rejected.code(), DotenvWorkerErrorCode::ContinuationLimit);
        assert_eq!(rejected.correlation().operation_id(), 99);
        assert_eq!(rejected.into_payload(), 99);
        assert_eq!(
            worker.continuation_outstanding(),
            DOTENV_WORKER_CONTINUATION_MAX
        );
        {
            let (lock, ready) = &*gate;
            *lock.lock().unwrap() = true;
            ready.notify_all();
        }

        for value in 0..DOTENV_WORKER_CONTINUATION_MAX as u64 {
            let outcome = recv_worker_outcome(&mut worker);
            assert!(outcome.is_continuation());
            assert_eq!(outcome.operation_id(), value + 1);
            assert_eq!(outcome.into_result(), Ok(value));
        }
        assert_eq!(worker.continuation_outstanding(), 0);

        // The ID reservation is exact to outstanding work, so explicit caller-managed reuse is
        // admitted only after the previous outcome has been consumed.
        worker
            .request_continuation(DotenvWorkerCorrelation::new(2, 2, 1), 42)
            .unwrap();
        let reused = recv_worker_outcome(&mut worker);
        assert_eq!(reused.operation_id(), 1);
        assert_eq!(reused.into_result(), Ok(42));
        assert_eq!(worker.continuation_outstanding(), 0);
    }

    #[test]
    fn lazy_worker_idle_ttl은_resource를_회수하고_다음_request에서_재시작한다() {
        let factory_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_counter = std::sync::Arc::clone(&factory_calls);
        let mut worker = LazyDotenvWorker::with_idle_ttl(
            std::time::Duration::from_millis(10),
            move || {
                factory_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            |_: &mut (), value: u64| Ok(value),
            || {},
        );

        worker
            .request_state(DotenvWorkerCorrelation::new(1, 1, 1), 1)
            .unwrap();
        assert_eq!(recv_worker_outcome(&mut worker).into_result(), Ok(1));
        wait_until(|| !worker.is_thread_running());
        worker
            .request_state(DotenvWorkerCorrelation::new(1, 2, 2), 2)
            .unwrap();
        assert_eq!(recv_worker_outcome(&mut worker).into_result(), Ok(2));
        assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn lazy_worker_timeout과_request경합에도_accepted_job을_잃지_않는다() {
        let mut worker = LazyDotenvWorker::with_idle_ttl(
            std::time::Duration::from_millis(2),
            || Ok(()),
            |_: &mut (), value: u64| Ok(value),
            || {},
        );

        for operation in 0..64 {
            if operation > 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            worker
                .request_state(
                    DotenvWorkerCorrelation::new(1, operation, operation),
                    operation,
                )
                .unwrap();
            let outcome = recv_worker_outcome(&mut worker);
            assert_eq!(outcome.operation_id(), operation);
            assert_eq!(outcome.into_result(), Ok(operation));
        }
    }

    #[test]
    fn lazy_worker_drop은_channel을_닫고_thread_resource_drop까지_join한다() {
        struct DropProbe(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resource_drops = std::sync::Arc::clone(&drops);
        let mut worker = LazyDotenvWorker::new(
            move || Ok(DropProbe(std::sync::Arc::clone(&resource_drops))),
            |_: &mut DropProbe, value: u64| Ok(value),
            || {},
        );
        worker
            .request_state(DotenvWorkerCorrelation::new(1, 1, 1), 1)
            .unwrap();
        assert_eq!(recv_worker_outcome(&mut worker).into_result(), Ok(1));
        drop(worker);
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn lazy_worker_stale_generation_revision은_payload를_노출하지_않고_거부한다() {
        let mut worker =
            LazyDotenvWorker::new(|| Ok(()), |_: &mut (), value: u64| Ok(value), || {});
        worker
            .request_state(DotenvWorkerCorrelation::new(7, 11, 13), 17)
            .unwrap();
        let stale = recv_worker_outcome(&mut worker);
        assert_eq!(
            stale.into_current(7, 12).unwrap_err(),
            DotenvWorkerErrorCode::StaleOutcome
        );

        worker
            .request_state(DotenvWorkerCorrelation::new(7, 12, 14), 19)
            .unwrap();
        let current = recv_worker_outcome(&mut worker)
            .into_current(7, 12)
            .unwrap();
        assert_eq!(current.into_result(), Ok(19));
    }

    #[test]
    fn lazy_worker_debug는_correlation과_payload를_항상_redact한다() {
        let correlation = DotenvWorkerCorrelation::new(31_337, 41_337, 51_337);
        let correlation_debug = format!("{correlation:?}");
        for forbidden in ["31337", "41337", "51337"] {
            assert!(
                !correlation_debug.contains(forbidden),
                "{correlation_debug}"
            );
        }

        let replaced = ReplacedDotenvState {
            correlation,
            payload: "super-secret-replaced".to_owned(),
        };
        let rejected = DotenvWorkerSubmitError {
            code: DotenvWorkerErrorCode::ContinuationLimit,
            correlation,
            payload: "super-secret-rejected".to_owned(),
        };
        let outcome = DotenvWorkerOutcome {
            kind: DotenvWorkerRequestKind::Continuation,
            correlation,
            result: Ok("super-secret-outcome".to_owned()),
        };
        for (debug, forbidden) in [
            (format!("{replaced:?}"), "super-secret-replaced"),
            (format!("{rejected:?}"), "super-secret-rejected"),
            (format!("{outcome:?}"), "super-secret-outcome"),
        ] {
            assert!(!debug.contains(forbidden), "{debug}");
            assert!(debug.contains("REDACTED"), "{debug}");
        }
    }

    #[test]
    fn lazy_worker_resource_failure는_timer_retry하지_않는다() {
        let factory_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let execute_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_counter = std::sync::Arc::clone(&factory_calls);
        let execute_counter = std::sync::Arc::clone(&execute_calls);
        let mut worker = LazyDotenvWorker::with_idle_ttl(
            std::time::Duration::from_millis(5),
            move || -> Result<(), DotenvWorkerErrorCode> {
                factory_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(DotenvWorkerErrorCode::ResourceOpenFailed)
            },
            move |_: &mut (), value: u64| {
                execute_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(value)
            },
            || {},
        );
        worker
            .request_state(DotenvWorkerCorrelation::new(1, 1, 1), 1)
            .unwrap();
        assert_eq!(
            recv_worker_outcome(&mut worker).into_result(),
            Err(DotenvWorkerErrorCode::ResourceOpenFailed)
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(factory_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(execute_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn lazy_worker_source는_app_runtime_io_repaint_dependency가_없다() {
        let source = include_str!("dotenv_sync.rs");
        let worker_source = source
            .split_once("pub const DOTENV_WORKER_IDLE_TTL")
            .unwrap()
            .1
            .split_once("\n#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        for forbidden in [
            "Db::open",
            "KeyringSecretStore",
            "RuntimeCommand",
            "request_repaint",
            "tokio::",
            "std::fs::",
        ] {
            assert!(!worker_source.contains(forbidden), "forbidden: {forbidden}");
        }
        assert_eq!(worker_source.matches("sync_channel::<()>(1)").count(), 1);
        assert_eq!(
            worker_source
                .matches("sync_channel::<DotenvWorkerOutcome<O>>(1)")
                .count(),
            1
        );
    }

    impl DotenvRepository for Db {
        fn credential_secret_location(
            &mut self,
            credential_id: &str,
        ) -> anyhow::Result<Option<DotenvSecretLocation>> {
            Db::credential_secret_location(self, credential_id).map(|location| {
                location.map(|location| {
                    DotenvSecretLocation::new(location.keyring_service, location.keyring_username)
                })
            })
        }

        fn acknowledge_physical_secret_slot_deleted(
            &mut self,
            logical_id: &str,
            physical_slot: &str,
        ) -> anyhow::Result<()> {
            Db::acknowledge_physical_secret_slot_deleted(self, logical_id, physical_slot)
                .map(|_| ())
        }

        fn register_physical_secret_slot_staging(
            &mut self,
            logical_id: &str,
            physical_slot: &str,
        ) -> anyhow::Result<()> {
            Db::register_physical_secret_slot_staging(self, logical_id, physical_slot)
        }

        fn insert_credential_with_secret_slot(
            &mut self,
            draft: &DotenvCredentialDraft,
            physical_slot: &str,
        ) -> anyhow::Result<()> {
            Db::insert_credential_with_secret_slot(
                self,
                &storage::CredentialMeta {
                    id: draft.id().to_owned(),
                    provider: draft.provider().to_owned(),
                    label: draft.label().to_owned(),
                    credential_kind: draft.credential_kind().to_owned(),
                    masked_hint: draft.masked_hint().map(str::to_owned),
                    workspace_id: draft.workspace_id().map(str::to_owned),
                },
                physical_slot,
                None,
            )
        }

        fn publish_credential_secret_slot_cas(
            &mut self,
            logical_id: &str,
            expected_previous_pointer: &str,
            physical_slot: &str,
            masked_hint: Option<&str>,
        ) -> anyhow::Result<bool> {
            Db::publish_credential_secret_slot_cas(
                self,
                logical_id,
                expected_previous_pointer,
                physical_slot,
                None,
                masked_hint,
            )
        }

        fn delete_credential_if_unused_cas(
            &mut self,
            logical_id: &str,
            expected_pointer: &str,
        ) -> anyhow::Result<bool> {
            Db::delete_credential_if_unused_cas(self, logical_id, expected_pointer)
        }

        fn list_env_profiles(
            &mut self,
            workspace_id: &str,
            limit: usize,
        ) -> anyhow::Result<Vec<DotenvProfile>> {
            let rows = Db::list_env_profiles(self, workspace_id)?;
            anyhow::ensure!(rows.len() <= limit, "dotenv profile scan limit exceeded");
            Ok(rows
                .into_iter()
                .map(|row| DotenvProfile {
                    id: row.id,
                    kind: row.kind,
                })
                .collect())
        }

        fn insert_env_profile(
            &mut self,
            workspace_id: &str,
            name: &str,
            kind: &str,
        ) -> anyhow::Result<String> {
            Db::insert_env_profile(self, workspace_id, name, kind)
        }

        fn list_env_vars(
            &mut self,
            profile_id: &str,
            limit: usize,
        ) -> anyhow::Result<Vec<DotenvVariable>> {
            let rows = Db::list_env_vars(self, profile_id)?;
            anyhow::ensure!(rows.len() <= limit, "dotenv variable scan limit exceeded");
            Ok(rows
                .into_iter()
                .map(|row| DotenvVariable {
                    key: row.key,
                    value: row.value,
                })
                .collect())
        }

        fn list_dotenv_owned_credential_ids(
            &mut self,
            limit: usize,
        ) -> anyhow::Result<std::collections::HashSet<String>> {
            Ok(Db::list_credential_secret_records(self, limit)?
                .into_iter()
                .filter(|record| record.meta.provider == "env")
                .map(|record| record.meta.id)
                .collect())
        }

        fn plain_env_value_allowed(&mut self, key: &str, value: &str) -> bool {
            Db::validate_env_var_for_persistence(key, &EnvValue::Plain(value.to_owned())).is_ok()
        }

        fn upsert_env_var(
            &mut self,
            profile_id: &str,
            key: &str,
            value: &EnvValue,
        ) -> anyhow::Result<()> {
            Db::upsert_env_var(self, profile_id, key, value)
        }

        fn delete_env_var(&mut self, profile_id: &str, key: &str) -> anyhow::Result<()> {
            Db::delete_env_var(self, profile_id, key)
        }

        fn delete_env_profile(&mut self, profile_id: &str) -> anyhow::Result<()> {
            Db::delete_env_profile(self, profile_id)
        }
    }

    fn sync_workspace_dotenv_for_test(
        repository: &mut dyn DotenvRepository,
        secret_store: &dyn secret::SecretStore,
        redaction: &secret::RedactionService,
        workspace_id: &str,
        root: &Path,
    ) -> anyhow::Result<Option<DotenvSyncReport>> {
        let Some(plan) = load_workspace_dotenv_plan(root)? else {
            return Ok(None);
        };
        apply_workspace_dotenv_plan(repository, secret_store, redaction, workspace_id, plan)
            .map(Some)
    }

    fn env_files_test_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("deppy-env-files-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn env_files_선택한_파일만_지우면_다른_파일은_보존한다() {
        let dir = env_files_test_dir();
        std::fs::write(dir.join(".env"), "PORT=1000\n").unwrap();
        std::fs::write(dir.join(".env.local"), "PORT=2000\n").unwrap();
        write_env_var_in_file(&dir, Some(".env.local"), "PORT", None).unwrap();
        assert_eq!(
            read_merged_dotenv(&dir).unwrap().unwrap(),
            vec![("PORT".to_owned(), "1000".to_owned())]
        );
        assert!(write_env_var_in_file(&dir, Some("../.env"), "PORT", None).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn env_files_외부_수정은_교체전에_거부하고_원본을_보존한다() {
        let dir = env_files_test_dir();
        std::fs::write(dir.join(".env"), "PORT=1000\n").unwrap();
        let originals = read_env_file_set(&dir).unwrap();
        let candidates = vec![Some("PORT=2000\n".to_owned()), None];
        std::fs::write(dir.join(".env"), "PORT=3000\n").unwrap();
        assert!(commit_env_file_set(&dir, &originals, &candidates).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "PORT=3000\n"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn env_files_교체중_충돌은_실패로_남고_다른_원본을_되돌리지_않는다() {
        let dir = env_files_test_dir();
        for file in DOTENV_FILE_NAMES {
            std::fs::write(dir.join(file), "PORT=1000\n").unwrap();
        }
        let first = prepare_env_file(&dir.join(".env"), b"KEEP=1\n").unwrap();
        let second = prepare_env_file(&dir.join(".env.local"), b"").unwrap();
        first.commit(Some(Some("PORT=1000\n"))).unwrap();
        std::fs::write(dir.join(".env.local"), "PORT=3000\n").unwrap();
        assert!(second.commit(Some(Some("PORT=1000\n"))).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "KEEP=1\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".env.local")).unwrap(),
            "PORT=3000\n"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn env_files_원본없음과_빈파일을_구분하고_출처에는_값을_넣지_않는다() {
        let dir = env_files_test_dir();
        assert!(load_dotenv_sources(&dir).unwrap().files.is_empty());
        std::fs::write(dir.join(".env"), "").unwrap();
        let empty = load_dotenv_sources(&dir).unwrap();
        assert_eq!(empty.files, vec![".env"]);
        assert!(empty.keys.is_empty());
        std::fs::write(dir.join(".env"), "PORT=1000\nPORT=2000\n").unwrap();
        std::fs::write(dir.join(".env.local"), "PORT=3000\n").unwrap();
        assert_eq!(
            load_dotenv_sources(&dir).unwrap().keys["PORT"],
            vec![".env", ".env.local"]
        );
        assert!(write_env_var(&dir, "PORT", Some("1000\nOTHER=1")).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "PORT=1000\nPORT=2000\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn env_files_삭제는_두_파일의_중복값을_모두_제거한다() {
        let dir = std::env::temp_dir().join(format!("deppy-env-delete-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "PORT=1000\nKEEP=1\n").unwrap();
        std::fs::write(dir.join(".env.local"), "PORT=2000\nPORT=3000\n").unwrap();
        write_env_var(&dir, "PORT", None).unwrap();
        let merged = read_merged_dotenv(&dir).unwrap().unwrap();
        assert!(!merged.iter().any(|(key, _)| key == "PORT"));
        assert!(
            merged
                .iter()
                .any(|(key, value)| key == "KEEP" && value == "1")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn write_env_var는_라운드트립을_보존한다() {
        let dir = std::env::temp_dir().join(format!("deppy-wenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");
        std::fs::write(&env, "# comment\nKEEP=1\nOLD=x\n").unwrap();
        // 수정 + 추가(공백/내부 큰따옴표 값) + 삭제
        write_env_var(&dir, "OLD", Some("new value")).unwrap();
        write_env_var(&dir, "QUOTED", Some("a\"b")).unwrap();
        write_env_var(&dir, "KEEP", None).unwrap();
        let content = std::fs::read_to_string(&env).unwrap();
        assert!(content.starts_with("# comment\n"), "주석 보존");
        assert!(!content.contains("KEEP="), "삭제 반영");
        let parsed = parse_dotenv(&content);
        assert_eq!(
            parsed
                .iter()
                .find(|(k, _)| k == "OLD")
                .map(|(_, v)| v.as_str()),
            Some("new value")
        );
        assert_eq!(
            parsed
                .iter()
                .find(|(k, _)| k == "QUOTED")
                .map(|(_, v)| v.as_str()),
            Some("a\"b"),
            "내부 큰따옴표 라운드트립"
        );
        // 둘 다 포함한 값은 거부
        assert!(write_env_var(&dir, "BAD", Some("a\"b'c")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_env_var는_중복_정의를_하나로_정리하고_삭제는_전부_지운다() {
        let dir =
            std::env::temp_dir().join(format!("deppy-wenv-duplicates-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");
        std::fs::write(&env, "PORT=1000\nKEEP=1\nexport PORT=2000\n").unwrap();

        write_env_var(&dir, "PORT", Some("3000")).unwrap();
        let content = std::fs::read_to_string(&env).unwrap();
        assert_eq!(content.matches("PORT=").count(), 1);
        assert_eq!(
            parse_dotenv(&content)
                .into_iter()
                .find(|(key, _)| key == "PORT")
                .map(|(_, value)| value),
            Some("3000".to_owned())
        );

        std::fs::write(&env, "PORT=1000\nKEEP=1\nPORT=2000\n").unwrap();
        write_env_var(&dir, "PORT", None).unwrap();
        let content = std::fs::read_to_string(&env).unwrap();
        assert!(!parse_dotenv(&content).iter().any(|(key, _)| key == "PORT"));
        assert!(content.contains("KEEP=1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_env_var는_읽을_수_없는_utf8_파일을_덮어쓰지_않는다() {
        let dir =
            std::env::temp_dir().join(format!("deppy-wenv-invalid-utf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");
        let original = [0xff, 0xfe, b'=', b'1', b'\n'];
        std::fs::write(&env, original).unwrap();

        assert!(write_env_var(&dir, "PORT", Some("3000")).is_err());
        assert_eq!(std::fs::read(&env).unwrap(), original);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_env_write는_권한을_보존하고_temp를_남기지_않는다() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!(
            "deppy-wenv-atomic-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");
        std::fs::write(&env, "PORT=1000\n").unwrap();
        std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o640)).unwrap();

        write_env_var(&dir, "PORT", Some("3000")).unwrap();
        assert_eq!(
            std::fs::metadata(&env).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read_to_string(&env).unwrap(), "PORT=3000\n");
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("deppy-tmp")
        }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_env_write_교체실패는_원본과_temp_정리를_보장한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-wenv-atomic-failure-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(".env");
        std::fs::create_dir(&target).unwrap();

        assert!(atomic_write_env(&target, b"PORT=3000\n").is_err());
        assert!(target.is_dir());
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("deppy-tmp")
        }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_dotenv는_주석_export_따옴표를_처리한다() {
        let content = r#"
# comment
export DATABASE_URL=postgres://localhost/dev
API_KEY="sk-live-123"
EMPTY=
QUOTED='hello world'
PLAIN=value # trailing comment
QUOTED_COMMENT="bar" # comment
INVALID LINE
=nokey
"#;
        let parsed = parse_dotenv(content);
        assert_eq!(
            parsed,
            vec![
                (
                    "DATABASE_URL".to_owned(),
                    "postgres://localhost/dev".to_owned()
                ),
                ("API_KEY".to_owned(), "sk-live-123".to_owned()),
                ("EMPTY".to_owned(), String::new()),
                ("QUOTED".to_owned(), "hello world".to_owned()),
                ("PLAIN".to_owned(), "value".to_owned()),
                ("QUOTED_COMMENT".to_owned(), "bar".to_owned()),
            ]
        );
    }

    #[test]
    fn dotenv_plan은_민감값을_debug에_노출하지_않는다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-plan-redaction-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "API_TOKEN=do-not-log\nPLAIN=ok\n").unwrap();

        let plan = load_workspace_dotenv_plan(&dir).unwrap().unwrap();
        assert_eq!(plan.entry_count(), 2);
        assert_eq!(plan.value_bytes(), "do-not-log".len() + "ok".len());
        let debug = format!("{plan:?}");
        assert!(debug.contains("entries: 2"), "{debug}");
        assert!(!debug.contains("do-not-log"), "{debug}");
        assert!(!debug.contains("API_TOKEN"), "{debug}");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dotenv_contract는_concrete_storage와_keyring_구현을_노출하지_않는다() {
        let source = include_str!("dotenv_sync.rs");
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests")
            .map(|(production, _)| production)
            .expect("test module marker");
        for forbidden in ["crate::storage", "storage::Db", "KeyringSecretStore"] {
            assert!(
                !production.contains(forbidden),
                "forbidden edge: {forbidden}"
            );
        }

        let location = DotenvSecretLocation::new(
            "do-not-log-service".to_owned(),
            "do-not-log-physical-slot".to_owned(),
        );
        let draft = DotenvCredentialDraft {
            id: "do-not-log-id".to_owned(),
            provider: "env".to_owned(),
            label: "API_TOKEN (.env)".to_owned(),
            credential_kind: "api_key".to_owned(),
            masked_hint: Some("do-not-log-hint".to_owned()),
            workspace_id: Some("do-not-log-workspace".to_owned()),
        };
        for debug in [format!("{location:?}"), format!("{draft:?}")] {
            assert!(debug.contains("REDACTED"), "{debug}");
            assert!(!debug.contains("do-not-log"), "{debug}");
            assert!(!debug.contains("API_TOKEN"), "{debug}");
        }
    }

    #[test]
    fn bounded_dotenv는_byte_item_key_value_hard_cap을_강제한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-hard-limits-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");

        std::fs::write(&env, vec![b'#'; DOTENV_TOTAL_BYTES_MAX + 1]).unwrap();
        assert_eq!(
            read_merged_dotenv(&dir).unwrap_err().to_string(),
            ERROR_DOTENV_BYTES
        );

        std::fs::write(
            &env,
            format!("{}=value\n", "K".repeat(DOTENV_KEY_BYTES_MAX + 1)),
        )
        .unwrap();
        assert_eq!(
            read_merged_dotenv(&dir).unwrap_err().to_string(),
            "dotenv_key_bytes_exceeded"
        );

        std::fs::write(
            &env,
            format!("KEY={}\n", "v".repeat(DOTENV_VALUE_BYTES_MAX + 1)),
        )
        .unwrap();
        assert_eq!(
            read_merged_dotenv(&dir).unwrap_err().to_string(),
            "dotenv_value_bytes_exceeded"
        );

        let duplicate_entries = "A=1\n".repeat(DOTENV_ENTRIES_MAX + 1);
        std::fs::write(&env, duplicate_entries).unwrap();
        assert_eq!(
            read_merged_dotenv(&dir).unwrap_err().to_string(),
            "dotenv_entries_exceeded"
        );

        let maximum_unique = (0..DOTENV_ENTRIES_MAX)
            .map(|index| format!("K{index}=v\n"))
            .collect::<String>();
        std::fs::write(&env, maximum_unique).unwrap();
        assert_eq!(
            read_merged_dotenv(&dir).unwrap().unwrap().len(),
            DOTENV_ENTRIES_MAX
        );

        let original = "KEEP=1\n";
        std::fs::write(&env, original).unwrap();
        assert!(write_env_var(&dir, &"K".repeat(DOTENV_KEY_BYTES_MAX + 1), Some("value")).is_err());
        assert!(write_env_var(&dir, "KEY", Some(&"v".repeat(DOTENV_VALUE_BYTES_MAX + 1))).is_err());
        assert_eq!(std::fs::read_to_string(&env).unwrap(), original);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dotenv_editor_caps_retained_comment_and_blank_lines() {
        let exact = "#\n".repeat(DOTENV_LINES_MAX);
        assert_eq!(
            collect_dotenv_lines_bounded(&exact).unwrap().len(),
            DOTENV_LINES_MAX
        );
        let plus_one = "#\n".repeat(DOTENV_LINES_MAX + 1);
        assert_eq!(
            collect_dotenv_lines_bounded(&plus_one)
                .unwrap_err()
                .to_string(),
            "dotenv_lines_exceeded"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dotenv_source_reader_rejects_symlink_and_special_file() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-types-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let regular = dir.join("regular");
        let link = dir.join(".env");
        std::fs::write(&regular, b"A=1\n").unwrap();
        symlink(&regular, &link).unwrap();
        assert_eq!(
            read_merged_dotenv(&dir).unwrap_err().to_string(),
            "dotenv_file_type_invalid"
        );
        let mut remaining = DOTENV_TOTAL_BYTES_MAX;
        assert_eq!(
            read_dotenv_file_app_bounded(Path::new("/dev/null"), &mut remaining)
                .unwrap_err()
                .to_string(),
            "dotenv_file_type_invalid"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn gitignore_reader_accepts_exact_and_rejects_plus_one_and_invalid_utf8() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-gitignore-bound-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let gitignore = dir.join(".gitignore");
        let mut exact = vec![b'#'; GITIGNORE_BYTES_MAX - b"\n.env*\n".len()];
        exact.extend_from_slice(b"\n.env*\n");
        assert_eq!(exact.len(), GITIGNORE_BYTES_MAX);
        std::fs::write(&gitignore, &exact).unwrap();
        ensure_env_gitignored(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&gitignore).unwrap().len(),
            exact.len() as u64
        );

        std::fs::write(&gitignore, vec![b'#'; GITIGNORE_BYTES_MAX + 1]).unwrap();
        assert_eq!(
            ensure_env_gitignored(&dir).unwrap_err().to_string(),
            "dotenv_gitignore_bytes_exceeded"
        );
        std::fs::write(&gitignore, [0xff]).unwrap();
        assert_eq!(
            ensure_env_gitignored(&dir).unwrap_err().to_string(),
            "dotenv_gitignore_utf8_invalid"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn gitignore_reader_rejects_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "deppy-gitignore-link-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, b"keep\n").unwrap();
        symlink(&target, dir.join(".gitignore")).unwrap();
        assert_eq!(
            ensure_env_gitignored(&dir).unwrap_err().to_string(),
            "dotenv_gitignore_type_invalid"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"keep\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn production_dotenv_inputs_have_bounded_read_and_line_collection_laws() {
        let production = include_str!("dotenv_sync.rs")
            .split_once("\n#[cfg(test)]\nmod tests")
            .map(|(production, _)| production)
            .unwrap();
        assert!(!production.contains("read_to_string"));
        assert!(!production.contains("content.lines().map(str::to_owned).collect"));
        assert!(production.contains(".take(probe as u64)"));
        assert!(production.contains("DOTENV_LINES_MAX"));
        assert!(production.contains("GITIGNORE_BYTES_MAX"));
        assert!(production.contains("dotenv_gitignore_sync_failed"));
    }

    #[test]
    fn 두_dotenv_파일은_하나의_1mib_byte_budget을_공유한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-aggregate-limit-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let half = DOTENV_TOTAL_BYTES_MAX / 2;
        std::fs::write(dir.join(".env"), vec![b'#'; half]).unwrap();
        std::fs::write(dir.join(".env.local"), vec![b'#'; half]).unwrap();
        assert!(read_merged_dotenv(&dir).unwrap().unwrap().is_empty());

        let half_plus_one = DOTENV_TOTAL_BYTES_MAX / 2 + 1;
        std::fs::write(dir.join(".env"), vec![b'#'; half_plus_one]).unwrap();
        std::fs::write(dir.join(".env.local"), vec![b'#'; half_plus_one]).unwrap();

        assert_eq!(
            read_merged_dotenv(&dir).unwrap_err().to_string(),
            ERROR_DOTENV_BYTES
        );
        assert!(write_env_var(&dir, "PORT", Some("3000")).is_err());
        assert_eq!(
            std::fs::metadata(dir.join(".env")).unwrap().len(),
            u64::try_from(half_plus_one).unwrap()
        );

        std::fs::write(dir.join(".env"), "A=1\n".repeat(DOTENV_ENTRIES_MAX / 2)).unwrap();
        std::fs::write(
            dir.join(".env.local"),
            "A=2\n".repeat(DOTENV_ENTRIES_MAX / 2 + 1),
        )
        .unwrap();
        assert_eq!(
            read_merged_dotenv(&dir).unwrap_err().to_string(),
            "dotenv_entries_exceeded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 테스트용 in-memory secret store. Delete failure injection models a crash/interrupted
    /// keyring cleanup after the database pointer has already been retired.
    struct MemStore {
        entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
        fail_delete: std::sync::atomic::AtomicBool,
    }

    impl MemStore {
        fn new() -> Self {
            Self {
                entries: std::sync::Mutex::new(std::collections::HashMap::new()),
                fail_delete: std::sync::atomic::AtomicBool::new(false),
            }
        }

        fn value(&self, id: &str) -> Option<String> {
            self.entries.lock().unwrap().get(id).cloned()
        }

        fn is_empty(&self) -> bool {
            self.entries.lock().unwrap().is_empty()
        }

        fn set_delete_failure(&self, fail: bool) {
            self.fail_delete
                .store(fail, std::sync::atomic::Ordering::Release);
        }
    }

    impl secret::SecretStore for MemStore {
        fn set_secret(&self, id: &str, s: &secret::SecretString) -> anyhow::Result<()> {
            self.entries
                .lock()
                .unwrap()
                .insert(id.to_owned(), s.expose().to_owned());
            Ok(())
        }
        fn get_secret(&self, id: &str) -> anyhow::Result<secret::SecretString> {
            self.entries
                .lock()
                .unwrap()
                .get(id)
                .map(|v| secret::SecretString::new(v.clone()))
                .ok_or_else(|| anyhow::anyhow!("없음"))
        }
        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            anyhow::ensure!(
                !self.fail_delete.load(std::sync::atomic::Ordering::Acquire),
                "injected delete failure"
            );
            self.entries.lock().unwrap().remove(id);
            Ok(())
        }
        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.entries.lock().unwrap().contains_key(id))
        }
    }

    fn physical_slot_for(db: &Db, credential_id: &str) -> secret::PhysicalSecretSlot {
        let location = db
            .credential_secret_location(credential_id)
            .unwrap()
            .expect("credential location");
        assert_eq!(location.keyring_service, secret::KEYRING_SERVICE);
        let logical = secret::LogicalCredentialId::new(credential_id.to_owned()).unwrap();
        let physical = secret::PhysicalSecretSlot::parse(location.keyring_username).unwrap();
        assert!(physical.belongs_to(&logical));
        assert_ne!(physical.as_str(), logical.as_str());
        physical
    }

    fn seed_versioned_credential(
        db: &Db,
        store: &MemStore,
        credential_id: &str,
        provider: &str,
        workspace_id: &str,
        value: Option<&str>,
    ) -> secret::PhysicalSecretSlot {
        let logical = secret::LogicalCredentialId::new(credential_id.to_owned()).unwrap();
        let plan = secret::SecretBundleStagePlan::allocate(logical, None).unwrap();
        db.register_physical_secret_slot_staging(
            plan.logical_id().as_str(),
            plan.new_slot().as_str(),
        )
        .unwrap();
        let staged_value = secret::SecretString::new(value.unwrap_or("missing-seed").to_owned());
        secret::stage_secret_bundle(
            store,
            &plan,
            secret::SecretBundleRef::new(&staged_value, None, None),
        )
        .unwrap();
        db.insert_credential_with_secret_slot(
            &storage::CredentialMeta {
                id: credential_id.to_owned(),
                provider: provider.to_owned(),
                label: "fixture".to_owned(),
                credential_kind: "api_key".to_owned(),
                masked_hint: None,
                workspace_id: Some(workspace_id.to_owned()),
            },
            plan.new_slot().as_str(),
            None,
        )
        .unwrap();
        if value.is_none() {
            secret::delete_secret_bundle(store, plan.new_slot()).unwrap();
        }
        plan.new_slot().clone()
    }

    #[test]
    fn legacy_logical_pointer는_fail_closed하고_error에_coordinate를_노출하지_않는다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-legacy-pointer-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let marker = "hostile-logical-coordinate-marker";
        db.insert_credential(&storage::CredentialMeta {
            id: marker.to_owned(),
            provider: "legacy".to_owned(),
            label: "fixture".to_owned(),
            credential_kind: "api_key".to_owned(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();

        let error = match resolve_secret_slot(&mut db, marker) {
            Ok(_) => panic!("legacy logical pointer must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), ERROR_SECRET_LOCATION_PHYSICAL);
        assert_eq!(secret_error_code(&error), ERROR_SECRET_LOCATION_PHYSICAL);
        assert!(!format!("{error:?}").contains(marker));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync는_생성_갱신_삭제를_수행하고_secret은_keyring으로_보낸다() {
        let dir = std::env::temp_dir().join(format!("deppy-dotenv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        // 1차: plain + secret 생성
        std::fs::write(dir.join(".env"), "PORT=3000\nAPI_KEY=sk-123\n").unwrap();
        let report = sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir)
            .unwrap()
            .unwrap();
        assert_eq!(report.upserted, 2);
        let profile = db
            .list_env_profiles(&ws)
            .unwrap()
            .into_iter()
            .find(|p| p.kind == DOTENV_PROFILE_KIND)
            .expect("dotenv profile 생성됨");
        let vars = db.list_env_vars(&profile.id).unwrap();
        assert_eq!(vars.len(), 2);
        let api = vars.iter().find(|v| v.key == "API_KEY").unwrap();
        let EnvValue::Secret { credential_id } = &api.value else {
            panic!("API_KEY는 secret이어야 함");
        };
        // 값은 DB가 아니라 store(keyring)에.
        let first_slot = physical_slot_for(&db, credential_id);
        assert_eq!(store.value(first_slot.as_str()).as_deref(), Some("sk-123"));
        assert_eq!(
            store.value(credential_id),
            None,
            "logical keyring mirror 금지"
        );

        // 2차: secret 값 갱신은 새 physical slot을 CAS publish하고 이전 bundle/ledger를 정리.
        std::fs::write(dir.join(".env"), "PORT=4000\nAPI_KEY=sk-456\n").unwrap();
        let report = sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir)
            .unwrap()
            .unwrap();
        assert_eq!((report.upserted, report.removed), (2, 0));
        let second_slot = physical_slot_for(&db, credential_id);
        assert_ne!(second_slot, first_slot);
        assert_eq!(store.value(first_slot.as_str()), None, "old bundle 삭제");
        assert_eq!(store.value(second_slot.as_str()).as_deref(), Some("sk-456"));
        let rows = db.physical_secret_slots_for_reconciliation(8).unwrap();
        assert_eq!(rows.len(), 1, "old orphan ledger acknowledgement");
        assert_eq!(rows[0].state, ::storage::PhysicalSecretSlotState::Published);

        // 동일 값 재동기화는 새 slot/DB write를 만들지 않는다.
        let report = sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir)
            .unwrap()
            .unwrap();
        assert_eq!((report.upserted, report.removed), (0, 0));
        assert_eq!(physical_slot_for(&db, credential_id), second_slot);

        // 3차: 키 삭제 → profile 반영, credential/keyring/ledger 정리.
        std::fs::write(dir.join(".env"), "PORT=4000\n").unwrap();
        let report = sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir)
            .unwrap()
            .unwrap();
        assert_eq!((report.upserted, report.removed), (0, 1));
        let vars = db.list_env_vars(&profile.id).unwrap();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].value, EnvValue::Plain("4000".to_owned()));
        assert!(store.is_empty(), "keyring 정리됨");
        assert!(
            db.physical_secret_slots_for_reconciliation(8)
                .unwrap()
                .is_empty()
        );

        // .env 없으면 profile 보존
        std::fs::remove_file(dir.join(".env")).unwrap();
        assert!(
            sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir)
                .unwrap()
                .is_none()
        );
        assert_eq!(db.list_env_vars(&profile.id).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redaction_corpus를_확보하지_못하면_secret_persistence를_실행하지_않는다() {
        struct FixedClock;
        impl secret::RedactionClock for FixedClock {
            fn now(&self) -> std::time::Duration {
                std::time::Duration::ZERO
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-redaction-closed-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::with_clock(
            secret::RedactionCorpusLimits {
                max_items: 1,
                max_bytes: 1,
            },
            std::time::Duration::ZERO,
            std::sync::Arc::new(FixedClock),
        )
        .unwrap();
        let workspace_id = db.create_workspace("test").unwrap();
        std::fs::write(dir.join(".env"), "API_KEY=must-not-reach-keyring\n").unwrap();

        let error =
            sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &workspace_id, &dir)
                .unwrap_err();

        assert_eq!(error.to_string(), ERROR_SECRET_REDACTION);
        assert!(store.entries.lock().unwrap().is_empty());
        assert!(db.list_credentials().unwrap().is_empty());
        assert!(
            db.list_env_profiles(&workspace_id)
                .unwrap()
                .into_iter()
                .all(|profile| profile.kind != DOTENV_PROFILE_KIND),
            "redaction preflight must fail before profile creation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 두번째_secret_redaction_preflight_실패도_첫_mutation전에_전체를_거부한다() {
        struct FixedClock;
        impl secret::RedactionClock for FixedClock {
            fn now(&self) -> std::time::Duration {
                std::time::Duration::ZERO
            }
        }

        let first_value = "first-secret-value-that-fits-exactly";
        let probe = secret::RedactionService::with_clock(
            secret::RedactionCorpusLimits {
                max_items: 1_024,
                max_bytes: 1024 * 1024,
            },
            std::time::Duration::from_secs(60),
            std::sync::Arc::new(FixedClock),
        )
        .unwrap();
        let first = secret::SecretString::new(first_value.to_owned());
        let _probe_lease = probe.acquire_rotating(&first).unwrap();
        let exact_first = probe.corpus_stats();
        assert!(exact_first.items > 0 && exact_first.bytes > 0);

        let redaction = secret::RedactionService::with_clock(
            secret::RedactionCorpusLimits {
                max_items: exact_first.items,
                max_bytes: exact_first.bytes,
            },
            std::time::Duration::from_secs(60),
            std::sync::Arc::new(FixedClock),
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-redaction-preflight-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let workspace_id = db.create_workspace("test").unwrap();
        std::fs::write(
            dir.join(".env"),
            format!("API_KEY_ONE={first_value}\nAPI_KEY_TWO=second-secret-value-must-overflow\n"),
        )
        .unwrap();

        let error =
            sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &workspace_id, &dir)
                .unwrap_err();

        assert_eq!(error.to_string(), ERROR_SECRET_REDACTION);
        assert!(store.is_empty());
        assert!(db.list_credentials().unwrap().is_empty());
        assert!(
            db.list_env_profiles(&workspace_id)
                .unwrap()
                .into_iter()
                .all(|profile| profile.kind != DOTENV_PROFILE_KIND)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 보호할_수_없는_값은_그_항목만_제외되고_동기화는_계속된다() {
        // 2026-08-21 사용자 보고: `.env`에 `GITHUB_OAUTH_CLIENT_ID=`(빈 값)가 있으면
        // 그 워크스페이스에서 빈 터미널조차 열리지 않았다. 키 이름이 secret으로
        // 분류되는데("OAUTH"가 AUTH를 포함) 빈 값은 redaction 최소 길이(6바이트)에
        // 걸려 동기화 전체가 fail-closed로 죽었기 때문이다.
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-short-secret-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let workspace_id = db.create_workspace("test").unwrap();
        std::fs::write(
            dir.join(".env"),
            // 빈 secret 값 둘 + 정상 길이 하나.
            "GITHUB_OAUTH_CLIENT_ID=\nGITHUB_OAUTH_CLIENT_SECRET=\nAUTH_SECRET=long-enough-to-redact\n",
        )
        .unwrap();

        let report =
            sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &workspace_id, &dir)
                .expect("빈 값 때문에 동기화 전체가 실패하면 안 된다");

        assert!(report.is_some(), "프로필이 만들어져야 한다");
        assert!(
            db.list_env_profiles(&workspace_id)
                .unwrap()
                .into_iter()
                .any(|profile| profile.kind == DOTENV_PROFILE_KIND),
            "dotenv 프로필이 남아야 한다"
        );

        // 보호할 수 없는 값이 섞여도 **나머지는 동기화된다**(2026-08-21). 예전엔 그
        // 한 줄이 동기화 전체를 죽여 워크스페이스가 통째로 막혔다. 그 값은 평문으로
        // 강등되지도 않는다 — 마스킹할 수 없는 값을 SQLite에 평문으로 남기면 로그로도
        // 샌다. 제외하고, 어떤 키였는지 리포트로 올린다.
        std::fs::write(
            dir.join(".env"),
            "DB_PWD=1234\nKEEP_ME=plain-value\nGOOD_SECRET=long-enough-to-redact\n",
        )
        .unwrap();
        let report =
            sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &workspace_id, &dir)
                .expect("보호 못 하는 값 하나가 동기화 전체를 죽이면 안 된다")
                .expect("변경이 있으므로 리포트가 있어야 한다");
        assert_eq!(
            report.skipped_keys,
            vec!["DB_PWD".to_owned()],
            "보호할 수 없는 키는 제외 목록에 올라야 한다"
        );

        let profile_id = db
            .list_env_profiles(&workspace_id)
            .unwrap()
            .into_iter()
            .find(|profile| profile.kind == DOTENV_PROFILE_KIND)
            .expect("dotenv 프로필")
            .id;
        let keys = db
            .list_env_vars(&profile_id)
            .unwrap()
            .into_iter()
            .map(|var| var.key)
            .collect::<Vec<_>>();
        assert!(
            keys.contains(&"KEEP_ME".to_owned()) && keys.contains(&"GOOD_SECRET".to_owned()),
            "나머지 항목은 반영돼야 한다: {keys:?}"
        );
        assert!(
            !keys.contains(&"DB_PWD".to_owned()),
            "보호 못 하는 값은 평문으로도 저장되면 안 된다: {keys:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 백회_rotation뒤에도_keyring_ledger_redaction_corpus가_증가하지_않는다() {
        struct FixedClock;
        impl secret::RedactionClock for FixedClock {
            fn now(&self) -> std::time::Duration {
                std::time::Duration::ZERO
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-rotation-soak-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::with_clock(
            secret::RedactionCorpusLimits::PRODUCTION,
            std::time::Duration::ZERO,
            std::sync::Arc::new(FixedClock),
        )
        .unwrap();
        let ws = db.create_workspace("test").unwrap();

        let mut logical_id = None;
        for generation in 0..100 {
            std::fs::write(
                dir.join(".env"),
                format!("API_KEY=rotation-secret-{generation:03}\n"),
            )
            .unwrap();
            sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir).unwrap();
            let profile = db
                .list_env_profiles(&ws)
                .unwrap()
                .into_iter()
                .find(|profile| profile.kind == DOTENV_PROFILE_KIND)
                .unwrap();
            let current_id = match &db.list_env_vars(&profile.id).unwrap()[0].value {
                EnvValue::Secret { credential_id } => credential_id.clone(),
                EnvValue::Plain(_) => panic!("secret fixture"),
            };
            if let Some(first) = logical_id.as_ref() {
                assert_eq!(&current_id, first, "logical identity must remain stable");
            } else {
                logical_id = Some(current_id.clone());
            }
            let physical = physical_slot_for(&db, &current_id);
            assert_eq!(store.entries.lock().unwrap().len(), 1);
            assert!(store.value(physical.as_str()).is_some());
            assert_eq!(
                store.value(&current_id),
                None,
                "logical keyring mirror 금지"
            );
            let rows = db.physical_secret_slots_for_reconciliation(8).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].state, ::storage::PhysicalSecretSlotState::Published);
        }

        let stats = redaction.corpus_stats();
        assert_eq!(stats.items, 0);
        assert_eq!(stats.bytes, 0);
        assert_eq!(stats.permanent_items, 0);
        assert_eq!(stats.active_leases, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_삭제_실패는_orphan_ledger를_남겨_startup에서_정확히_재시도한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-delete-failure-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        std::fs::write(dir.join(".env"), "API_KEY=sk-delete-failure\n").unwrap();
        sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir).unwrap();
        let profile = db
            .list_env_profiles(&ws)
            .unwrap()
            .into_iter()
            .find(|profile| profile.kind == DOTENV_PROFILE_KIND)
            .unwrap();
        let credential_id = match &db.list_env_vars(&profile.id).unwrap()[0].value {
            EnvValue::Secret { credential_id } => credential_id.clone(),
            EnvValue::Plain(_) => panic!("secret fixture"),
        };
        let physical = physical_slot_for(&db, &credential_id);

        store.set_delete_failure(true);
        std::fs::write(dir.join(".env"), "").unwrap();
        let report = sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir)
            .unwrap()
            .unwrap();
        assert_eq!(report.removed, 1);
        assert!(
            db.credential_secret_location(&credential_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.value(physical.as_str()).as_deref(),
            Some("sk-delete-failure")
        );
        assert_eq!(
            store.value(&credential_id),
            None,
            "logical keyring mirror 금지"
        );
        let rows = db.physical_secret_slots_for_reconciliation(8).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, ::storage::PhysicalSecretSlotState::Orphan);
        assert_eq!(rows[0].physical_slot, physical.as_str());

        // Startup reconciliation consumes the durable exact coordinate. Delete and acknowledgement
        // are idempotent, so an interrupted first attempt never guesses or retries a logical id.
        store.set_delete_failure(false);
        secret::delete_secret_bundle(&store, &physical).unwrap();
        assert!(
            db.acknowledge_physical_secret_slot_deleted(&credential_id, physical.as_str())
                .unwrap()
        );
        assert!(store.is_empty());
        assert!(
            db.physical_secret_slots_for_reconciliation(8)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_dotenv_제거는_env_owned_slot만_cas_정리한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-remove-workspace-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        std::fs::write(dir.join(".env"), "API_KEY=sk-owned\n").unwrap();
        sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir).unwrap();
        let profile = db
            .list_env_profiles(&ws)
            .unwrap()
            .into_iter()
            .find(|profile| profile.kind == DOTENV_PROFILE_KIND)
            .unwrap();
        let owned_id = match &db.list_env_vars(&profile.id).unwrap()[0].value {
            EnvValue::Secret { credential_id } => credential_id.clone(),
            EnvValue::Plain(_) => panic!("secret fixture"),
        };
        let owned_slot = physical_slot_for(&db, &owned_id);

        let custom_id = "custom-retained";
        let custom_slot =
            seed_versioned_credential(&db, &store, custom_id, "custom", &ws, Some("custom-secret"));
        db.upsert_env_var(
            &profile.id,
            "CUSTOM_TOKEN",
            &EnvValue::Secret {
                credential_id: custom_id.to_owned(),
            },
        )
        .unwrap();

        assert_eq!(remove_workspace_dotenv(&mut db, &store, &ws).unwrap(), 2);
        assert!(
            !db.list_env_profiles(&ws)
                .unwrap()
                .iter()
                .any(|profile| profile.kind == DOTENV_PROFILE_KIND)
        );
        assert!(db.credential_secret_location(&owned_id).unwrap().is_none());
        assert_eq!(store.value(owned_slot.as_str()), None);
        assert!(db.credential_secret_location(custom_id).unwrap().is_some());
        assert_eq!(
            store.value(custom_slot.as_str()).as_deref(),
            Some("custom-secret")
        );
        assert_eq!(store.value(&owned_id), None, "logical keyring mirror 금지");
        assert_eq!(store.value(custom_id), None, "logical keyring mirror 금지");
        let rows = db.physical_secret_slots_for_reconciliation(8).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, ::storage::PhysicalSecretSlotState::Published);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_저장소면_env_기록_시_gitignore_보호가_추가된다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-gitignore-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        // 1) .gitignore 없음 → 생성 + .env/.env.local 추가
        write_env_var(&dir, "KTX_PASSWORD", Some("pw")).unwrap();
        let ignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(ignore.lines().any(|l| l.trim() == ".env"), "{ignore}");
        assert!(ignore.lines().any(|l| l.trim() == ".env.local"), "{ignore}");
        // 2) 재기록해도 중복 추가 없음(멱등)
        write_env_var(&dir, "KTX_PASSWORD", Some("pw2")).unwrap();
        let again = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(ignore, again, "중복 추가됨");
        // 3) 이미 커버 패턴(.env*)이 있으면 건드리지 않음
        let dir2 = dir.join("sub");
        std::fs::create_dir_all(dir2.join(".git")).unwrap();
        std::fs::write(dir2.join(".gitignore"), "node_modules/\n.env*\n").unwrap();
        write_env_var(&dir2, "PORT_HINT", Some("1")).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir2.join(".gitignore")).unwrap(),
            "node_modules/\n.env*\n"
        );
        // 4) git 저장소가 아니면 .gitignore를 만들지 않음
        let dir3 = dir.join("plain");
        std::fs::create_dir_all(&dir3).unwrap();
        write_env_var(&dir3, "PORT_HINT", Some("1")).unwrap();
        assert!(!dir3.join(".gitignore").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 레거시_profile_변수는_env_파일로_이전되고_resolve_실패는_보류된다() {
        use secret::SecretStore as _;
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-migrate-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let ws = db.create_workspace("legacy").unwrap();

        // 과거 UI가 만들던 DB 전용 profile: plain 1 + secret 2(하나는 keyring 값 없음).
        let legacy = db.insert_env_profile(&ws, "default", "local").unwrap();
        db.upsert_env_var(&legacy, "GREETING", &EnvValue::Plain("hello".into()))
            .unwrap();
        let mut slots = std::collections::HashMap::new();
        for (key, cred, seed) in [
            ("KTX_PASSWORD", "cred-ok", true),
            ("LOST_TOKEN", "cred-gone", false),
        ] {
            let slot = seed_versioned_credential(
                &db,
                &store,
                cred,
                "custom",
                &ws,
                seed.then_some("pw-1234"),
            );
            assert_eq!(store.value(cred), None, "logical keyring mirror 금지");
            slots.insert(cred, slot);
            db.upsert_env_var(
                &legacy,
                key,
                &EnvValue::Secret {
                    credential_id: cred.to_owned(),
                },
            )
            .unwrap();
        }

        let migrated = migrate_legacy_profiles_to_dotenv(&mut db, &store, &ws, &dir).unwrap();
        assert_eq!(migrated, 2, "plain + resolve 가능한 secret만 이전");
        let env = std::fs::read_to_string(dir.join(".env")).unwrap();
        assert!(env.contains("GREETING=hello"), "{env}");
        assert!(env.contains("KTX_PASSWORD=pw-1234"), "{env}");
        assert!(
            !env.contains("LOST_TOKEN"),
            "resolve 실패 키는 파일에 없음: {env}"
        );
        // resolve 실패 키는 legacy profile에 보류(값 유실 방지 — 다음 force가 재시도).
        let remaining = db.list_env_vars(&legacy).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].key, "LOST_TOKEN");
        assert!(
            db.list_env_profiles(&ws)
                .unwrap()
                .iter()
                .any(|p| p.id == legacy),
            "빈 profile만 삭제 — 보류 키가 있으면 유지"
        );

        // 보류 키의 keyring 값이 복구되면 다음 이전에서 마저 옮기고 profile을 정리한다.
        let lost_slot = slots.get("cred-gone").unwrap();
        store
            .set_secret(
                lost_slot.as_str(),
                &secret::SecretString::new("tok-9".to_owned()),
            )
            .unwrap();
        let migrated = migrate_legacy_profiles_to_dotenv(&mut db, &store, &ws, &dir).unwrap();
        assert_eq!(migrated, 1);
        assert!(
            std::fs::read_to_string(dir.join(".env"))
                .unwrap()
                .contains("LOST_TOKEN=tok-9")
        );
        assert!(
            !db.list_env_profiles(&ws)
                .unwrap()
                .iter()
                .any(|p| p.kind != DOTENV_PROFILE_KIND),
            "레거시 profile 정리됨"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_local이_env를_덮어쓰고_병합_기준으로_삭제한다() {
        let dir =
            std::env::temp_dir().join(format!("deppy-dotenv-merge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        // .env + .env.local — 겹치는 PORT는 .env.local이 이긴다.
        std::fs::write(dir.join(".env"), "PORT=3000\nFOO=a\n").unwrap();
        std::fs::write(dir.join(".env.local"), "PORT=5000\nBAR=b\n").unwrap();
        let report = sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir)
            .unwrap()
            .unwrap();
        assert_eq!(report.upserted, 3);
        let profile = db
            .list_env_profiles(&ws)
            .unwrap()
            .into_iter()
            .find(|p| p.kind == DOTENV_PROFILE_KIND)
            .unwrap();
        let vars = db.list_env_vars(&profile.id).unwrap();
        assert_eq!(vars.len(), 3);
        let port = vars.iter().find(|v| v.key == "PORT").unwrap();
        assert_eq!(port.value, EnvValue::Plain("5000".to_owned()));

        // .env.local 삭제 → PORT는 .env 값으로 복귀, BAR는 병합 결과에서 사라져 제거.
        std::fs::remove_file(dir.join(".env.local")).unwrap();
        let report = sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir)
            .unwrap()
            .unwrap();
        assert_eq!((report.upserted, report.removed), (1, 1));
        let vars = db.list_env_vars(&profile.id).unwrap();
        assert_eq!(vars.len(), 2);
        let port = vars.iter().find(|v| v.key == "PORT").unwrap();
        assert_eq!(port.value, EnvValue::Plain("3000".to_owned()));
        assert!(!vars.iter().any(|v| v.key == "BAR"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_local만_있어도_동기화한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-local-only-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        std::fs::write(dir.join(".env.local"), "PORT=7000\n").unwrap();
        let report = sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir)
            .unwrap()
            .unwrap();
        assert_eq!(report.upserted, 1);
        let profile = db
            .list_env_profiles(&ws)
            .unwrap()
            .into_iter()
            .find(|p| p.kind == DOTENV_PROFILE_KIND)
            .unwrap();
        let vars = db.list_env_vars(&profile.id).unwrap();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].value, EnvValue::Plain("7000".to_owned()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_local_읽기_실패는_부분_동기화하지_않고_기존_상태를_보존한다() {
        let dir = std::env::temp_dir().join(format!(
            "deppy-dotenv-read-failure-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        std::fs::write(dir.join(".env"), "PORT=3000\n").unwrap();
        std::fs::write(dir.join(".env.local"), "API_KEY=sk-local\n").unwrap();
        sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir).unwrap();
        let profile = db
            .list_env_profiles(&ws)
            .unwrap()
            .into_iter()
            .find(|profile| profile.kind == DOTENV_PROFILE_KIND)
            .unwrap();
        let before = db.list_env_vars(&profile.id).unwrap();
        assert_eq!(before.len(), 2);
        let credential_id = before
            .iter()
            .find_map(|var| match &var.value {
                EnvValue::Secret { credential_id } => Some(credential_id.clone()),
                EnvValue::Plain(_) => None,
            })
            .unwrap();

        std::fs::write(dir.join(".env.local"), [0xff, 0xfe, b'\n']).unwrap();
        assert!(sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir).is_err());
        assert_eq!(db.list_env_vars(&profile.id).unwrap(), before);
        let slot = physical_slot_for(&db, &credential_id);
        assert_eq!(store.value(slot.as_str()).as_deref(), Some("sk-local"));
        assert_eq!(
            store.value(&credential_id),
            None,
            "logical keyring mirror 금지"
        );

        // A valid first file followed by an aggregate over-limit second file must also leave the
        // previously committed database/keyring state untouched.
        let mut first = "#".repeat(600 * 1024);
        first.push_str("\nPORT=4000\n");
        std::fs::write(dir.join(".env"), first).unwrap();
        std::fs::write(dir.join(".env.local"), "#".repeat(500 * 1024)).unwrap();
        assert_eq!(
            sync_workspace_dotenv_for_test(&mut db, &store, &redaction, &ws, &dir)
                .unwrap_err()
                .to_string(),
            ERROR_DOTENV_BYTES
        );
        assert_eq!(db.list_env_vars(&profile.id).unwrap(), before);
        assert_eq!(store.value(slot.as_str()).as_deref(), Some("sk-local"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_secret_key는_민감_키를_넓게_잡는다() {
        for k in [
            "API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "DB_PASSWORD",
            "AUTH_DOMAIN",
            // 자격증명 포함 URL — 복원 경로 redaction을 위해 키 판정으로 승격(codex High,
            // 이전엔 validate_env_var_for_persistence 폴백이 잡았다).
            "DATABASE_URL",
            "DB_URL",
        ] {
            assert!(is_secret_key(k), "{k}는 secret이어야 함");
        }
        // `PRIVATE_KEY`는 잡되 `PRIVATE` 단독은 잡지 않는다(2026-08-21) — 저장소의
        // `secret_like_env_key`와 같은 기준이다.
        assert!(
            is_secret_key("SSH_PRIVATE_KEY"),
            "PRIVATE_KEY는 secret이어야 함"
        );
        for k in [
            "NODE_ENV",
            "PORT",
            "LOG_LEVEL",
            // 평범한 플래그가 비밀로 잡히면, 값이 짧을 때 dotenv 동기화 전체가
            // fail-closed로 죽어 워크스페이스가 통째로 막힌다(사용자 보고).
            "ALLOW_PRIVATE_URLS",
        ] {
            assert!(!is_secret_key(k), "{k}는 plain이어야 함");
        }
    }
}
