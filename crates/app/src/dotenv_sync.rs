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

use std::io::Write as _;
use std::path::Path;

use anyhow::Context;

use crate::env::EnvValue;
use crate::storage::Db;

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
    parse_dotenv_bounded, read_dotenv_file_bounded, read_dotenv_merged_bounded,
};

/// 동기화 결과 요약 (로그/알림용).
#[derive(Debug, Default, PartialEq)]
pub struct DotenvSyncReport {
    pub upserted: usize,
    pub removed: usize,
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

fn resolve_secret_slot(db: &Db, credential_id: &str) -> anyhow::Result<ResolvedSecretSlot> {
    let location = db
        .credential_secret_location(credential_id)
        .map_err(|_| static_secret_error(ERROR_SECRET_LOCATION_READ))?
        .ok_or_else(|| static_secret_error(ERROR_SECRET_LOCATION_MISSING))?;
    if location.keyring_service != secret::KEYRING_SERVICE {
        return Err(static_secret_error(ERROR_SECRET_LOCATION_SERVICE));
    }
    let logical = secret::LogicalCredentialId::new(credential_id.to_owned())
        .map_err(|_| static_secret_error(ERROR_SECRET_LOCATION_LOGICAL))?;
    let physical = secret::PhysicalSecretSlot::parse(location.keyring_username)
        .map_err(|_| static_secret_error(ERROR_SECRET_LOCATION_PHYSICAL))?;
    if !physical.belongs_to(&logical) {
        return Err(static_secret_error(ERROR_SECRET_LOCATION_OWNER));
    }
    Ok(ResolvedSecretSlot { logical, physical })
}

fn cleanup_secret_slot(
    db: &Db,
    secret_store: &dyn secret::SecretStore,
    slot: &ResolvedSecretSlot,
) -> anyhow::Result<()> {
    secret::delete_secret_bundle(secret_store, &slot.physical)
        .map_err(|_| static_secret_error(ERROR_SECRET_BUNDLE_DELETE))?;
    db.acknowledge_physical_secret_slot_deleted(slot.logical.as_str(), slot.physical.as_str())
        .map_err(|_| static_secret_error(ERROR_SECRET_LEDGER_ACK))?;
    Ok(())
}

fn cleanup_secret_slot_best_effort(
    db: &Db,
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
    if db
        .acknowledge_physical_secret_slot_deleted(slot.logical.as_str(), slot.physical.as_str())
        .is_err()
    {
        warn_secret_failure(phase, ERROR_SECRET_LEDGER_ACK);
    }
}

fn stage_access_only_secret(
    db: &Db,
    secret_store: &dyn secret::SecretStore,
    logical: secret::LogicalCredentialId,
    previous: Option<secret::PhysicalSecretSlot>,
    value: &secret::SecretString,
) -> anyhow::Result<ResolvedSecretSlot> {
    let plan = secret::SecretBundleStagePlan::allocate(logical, previous)
        .map_err(|_| static_secret_error(ERROR_SECRET_STAGE_PLAN))?;
    db.register_physical_secret_slot_staging(plan.logical_id().as_str(), plan.new_slot().as_str())
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
        cleanup_secret_slot_best_effort(db, secret_store, &staged_slot, "stage_rollback");
        return Err(static_secret_error(ERROR_SECRET_BUNDLE_STAGE));
    }
    Ok(staged_slot)
}

fn create_dotenv_credential(
    db: &Db,
    secret_store: &dyn secret::SecretStore,
    workspace_id: &str,
    key: &str,
    value: &secret::SecretString,
) -> anyhow::Result<ResolvedSecretSlot> {
    let logical = secret::LogicalCredentialId::new(uuid::Uuid::new_v4().to_string())
        .map_err(|_| static_secret_error(ERROR_SECRET_LOCATION_LOGICAL))?;
    let staged = stage_access_only_secret(db, secret_store, logical, None, value)?;
    let meta = crate::storage::CredentialMeta {
        id: staged.logical.as_str().to_owned(),
        provider: "env".to_owned(),
        label: format!("{key} (.env)"),
        credential_kind: "api_key".to_owned(),
        masked_hint: Some(secret::masked_hint(value.expose())),
        workspace_id: Some(workspace_id.to_owned()),
    };
    if db
        .insert_credential_with_secret_slot(&meta, staged.physical.as_str(), None)
        .is_err()
    {
        cleanup_secret_slot_best_effort(db, secret_store, &staged, "create_rollback");
        return Err(static_secret_error(ERROR_SECRET_CREATE_PUBLISH));
    }
    Ok(staged)
}

/// Rotate only when the access value actually changed. A missing/corrupt old bundle is repaired by
/// publishing a fresh access-only bundle. Once CAS publishes the new pointer, old cleanup is
/// best-effort because its durable orphan row is the crash-safe retry source.
fn rotate_dotenv_credential(
    db: &Db,
    secret_store: &dyn secret::SecretStore,
    credential_id: &str,
    value: &secret::SecretString,
) -> anyhow::Result<bool> {
    let previous = resolve_secret_slot(db, credential_id)?;
    if let Ok(bundle) = secret::read_secret_bundle(secret_store, &previous.physical)
        && bundle.access().expose() == value.expose()
        && bundle.refresh().is_none()
        && bundle.dcr().is_none()
    {
        return Ok(false);
    }

    let staged = stage_access_only_secret(
        db,
        secret_store,
        previous.logical.clone(),
        Some(previous.physical.clone()),
        value,
    )?;
    let published = match db.publish_credential_secret_slot_cas(
        previous.logical.as_str(),
        previous.physical.as_str(),
        staged.physical.as_str(),
        None,
        Some(&secret::masked_hint(value.expose())),
    ) {
        Ok(published) => published,
        Err(_) => {
            cleanup_secret_slot_best_effort(db, secret_store, &staged, "rotate_rollback");
            return Err(static_secret_error(ERROR_SECRET_ROTATE_PUBLISH));
        }
    };
    if !published {
        cleanup_secret_slot_best_effort(db, secret_store, &staged, "rotate_stale_cleanup");
        return Err(static_secret_error(ERROR_SECRET_ROTATE_STALE));
    }
    cleanup_secret_slot_best_effort(db, secret_store, &previous, "rotate_old_cleanup");
    Ok(true)
}

fn retire_dotenv_credential(
    db: &Db,
    secret_store: &dyn secret::SecretStore,
    target: &ResolvedSecretSlot,
) -> anyhow::Result<bool> {
    let deleted = db
        .delete_credential_if_unused_cas(target.logical.as_str(), target.physical.as_str())
        .map_err(|_| static_secret_error(ERROR_SECRET_DELETE_CAS))?;
    if !deleted {
        return Ok(false);
    }
    cleanup_secret_slot(db, secret_store, target)?;
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

fn atomic_write_env(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!(".env 상위 디렉터리 없음: {}", path.display()))?;
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
    let mut file = options
        .open(&temp)
        .with_context(|| format!("dotenv 임시 파일 생성 실패: {}", temp.display()))?;
    let mut guard = TempFileGuard(Some(temp.clone()));

    // 기존 파일의 접근 권한을 유지한다. 새 파일은 OpenOptions의 0600(Unix) 기본을 쓴다.
    match std::fs::metadata(path) {
        Ok(metadata) => {
            std::fs::set_permissions(&temp, metadata.permissions())
                .with_context(|| format!("dotenv 임시 파일 권한 설정 실패: {}", temp.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("dotenv 원본 권한 조회 실패: {}", path.display()));
        }
    }
    file.write_all(contents)
        .with_context(|| format!("dotenv 임시 파일 쓰기 실패: {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("dotenv 임시 파일 sync 실패: {}", temp.display()))?;
    drop(file);

    atomic_replace(&temp, path)
        .with_context(|| format!("dotenv 원자 교체 실패: {}", path.display()))?;
    guard.disarm();

    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("dotenv 디렉터리 sync 실패: {}", parent.display()))?;
    Ok(())
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

/// UI 편집을 `.env` 파일에 **라인 단위**로 반영한다(7·8번, 2026-07-10). 주석·순서 보존.
/// - 대상 파일: 키가 이미 있는 파일(.env.local 우선순위 역순으로 탐색), 없으면 `.env`
///   (파일이 없으면 생성). value=None이면 해당 라인 삭제.
/// - 반영 후 mtime 폴링/명시 sync가 DB를 따라 갱신한다(.env가 단일 진실).
pub fn write_env_var(root: &std::path::Path, key: &str, value: Option<&str>) -> anyhow::Result<()> {
    anyhow::ensure!(
        key.len() <= DOTENV_KEY_BYTES_MAX,
        "dotenv_key_bytes_exceeded"
    );
    if let Some(value) = value {
        anyhow::ensure!(
            value.len() <= DOTENV_VALUE_BYTES_MAX,
            "dotenv_value_bytes_exceeded"
        );
    }
    let key_probe = format!("{key}=");
    let valid_key = parse_dotenv_bounded(&key_probe, 1)?;
    anyhow::ensure!(
        valid_key.len() == 1 && valid_key[0].0 == key,
        "dotenv_key_invalid"
    );

    let mut remaining_bytes = DOTENV_TOTAL_BYTES_MAX;
    let mut contents: [Option<String>; DOTENV_FILE_NAMES.len()] = std::array::from_fn(|_| None);
    for (index, name) in DOTENV_FILE_NAMES.iter().enumerate() {
        contents[index] = read_dotenv_file_bounded(&root.join(name), &mut remaining_bytes)?;
    }
    let total_input_bytes = DOTENV_TOTAL_BYTES_MAX - remaining_bytes;

    let mut remaining_entries = DOTENV_ENTRIES_MAX;
    let mut contains_key = [false; DOTENV_FILE_NAMES.len()];
    for (index, content) in contents.iter().enumerate() {
        let Some(content) = content.as_deref() else {
            continue;
        };
        let parsed = parse_dotenv_bounded(content, remaining_entries)?;
        remaining_entries -= parsed.len();
        contains_key[index] = parsed.iter().any(|(existing, _)| existing == key);
    }

    // 키가 존재하는 파일 찾기 — 병합 우선순위가 높은 파일(.env.local)부터.
    let target_index = contains_key
        .iter()
        .rposition(|contains| *contains)
        .unwrap_or(0);
    let path = root.join(DOTENV_FILE_NAMES[target_index]);
    // NotFound만 새 파일로 취급한다. 권한 오류/잘못된 UTF-8/일시적 I/O 실패를 빈 파일로
    // 오인해 기존 .env 전체를 덮어쓰는 데이터 손실을 막는다.
    let content = contents[target_index].take().unwrap_or_default();

    // 값 직렬화 — 파서가 이스케이프를 해석하지 않으므로(codex Med) 이스케이프 금지:
    // 내부에 "가 있으면 '…'로 감싸고, "와 '를 둘 다 포함하면 라운드트립 불가라 거부.
    let render = |v: &str| -> anyhow::Result<String> {
        if v.contains('"') {
            anyhow::ensure!(
                !v.contains('\''),
                "큰따옴표와 작은따옴표를 모두 포함한 값은 .env에 기록할 수 없습니다"
            );
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

    let mut lines: Vec<String> = content.lines().map(str::to_owned).collect();
    let matches_key = |line: &str| -> bool {
        let t = line.trim();
        let t = t.strip_prefix("export ").unwrap_or(t).trim_start();
        t.split_once('=')
            .map(|(k, _)| k.trim() == key)
            .unwrap_or(false)
    };
    let existing: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| matches_key(line).then_some(idx))
        .collect();
    match (existing.last().copied(), value) {
        (Some(last), Some(v)) => {
            // dotenv의 실효 값은 마지막 중복 정의다. 마지막 행을 갱신하고 앞선 중복은
            // 제거해 UI 편집 직후에도 파서/셸에서 동일한 단일 값이 보이게 한다.
            lines[last] = render(v)?;
            for idx in existing[..existing.len() - 1].iter().rev() {
                lines.remove(*idx);
            }
        }
        (Some(_), None) => {
            // 하나만 지우면 뒤의 중복 정의가 살아나 삭제가 무효화되므로 전부 제거한다.
            lines.retain(|line| !matches_key(line));
        }
        (None, Some(v)) => lines.push(render(v)?),
        (None, None) => return Ok(()), // 지울 것 없음
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let next_total_bytes = total_input_bytes
        .checked_sub(content.len())
        .and_then(|bytes| bytes.checked_add(out.len()))
        .ok_or_else(|| static_secret_error(ERROR_DOTENV_BYTES))?;
    anyhow::ensure!(
        next_total_bytes <= DOTENV_TOTAL_BYTES_MAX,
        ERROR_DOTENV_BYTES
    );

    // Validate the exact post-write two-file aggregate before replacing either file.
    let mut remaining_entries = DOTENV_ENTRIES_MAX;
    for (index, existing) in contents.iter().enumerate() {
        let candidate = if index == target_index {
            Some(out.as_str())
        } else {
            existing.as_deref()
        };
        if let Some(candidate) = candidate {
            let parsed = parse_dotenv_bounded(candidate, remaining_entries)?;
            remaining_entries -= parsed.len();
        }
    }
    atomic_write_env(&path, out.as_bytes())
        .with_context(|| format!(".env 기록 실패: {}", path.display()))?;
    // 유출 방지(E2): deppy가 .env를 기록하는 유일한 지점 — git 저장소면 .gitignore
    // 보호를 함께 보장한다. best-effort(경고만) — 파일 기록 자체는 실패시키지 않는다.
    if ensure_env_gitignored(root).is_err() {
        tracing::warn!(
            kind = "dotenv_file",
            phase = "gitignore",
            error_code = "dotenv_gitignore_failed",
            "dotenv file protection failed"
        );
    }
    Ok(())
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
    let content = match std::fs::read_to_string(&gitignore) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!(".gitignore 읽기 실패: {}", gitignore.display()));
        }
    };
    let covers = |name: &str| -> bool {
        content.lines().map(str::trim).any(|line| {
            line == name
                || line == format!("/{name}")
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
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore)
        .with_context(|| format!(".gitignore 열기 실패: {}", gitignore.display()))?;
    file.write_all(block.as_bytes())
        .with_context(|| format!(".gitignore 기록 실패: {}", gitignore.display()))?;
    tracing::info!(added = ?missing, ".gitignore에 .env 보호 추가");
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
    db: &mut Db,
    secret_store: &dyn secret::SecretStore,
    workspace_id: &str,
    root: &Path,
) -> anyhow::Result<usize> {
    let legacy: Vec<_> = db
        .list_env_profiles(workspace_id)?
        .into_iter()
        .filter(|p| p.kind != DOTENV_PROFILE_KIND)
        .collect();
    if legacy.is_empty() {
        return Ok(0);
    }
    let mut migrated = 0usize;
    for profile in &legacy {
        let vars = db.list_env_vars(&profile.id)?;
        let mut remaining = vars.len();
        for var in &vars {
            let value = match &var.value {
                EnvValue::Plain(v) => v.clone(),
                EnvValue::Secret { credential_id } => {
                    let slot = match resolve_secret_slot(db, credential_id) {
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
            db.delete_env_var(&profile.id, &var.key)?;
            migrated += 1;
            remaining -= 1;
        }
        if remaining == 0 {
            db.delete_env_profile(&profile.id)?;
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
    db: &mut Db,
    secret_store: &dyn secret::SecretStore,
    workspace_id: &str,
) -> anyhow::Result<usize> {
    let Some(profile) = db
        .list_env_profiles(workspace_id)?
        .into_iter()
        .find(|p| p.kind == DOTENV_PROFILE_KIND)
    else {
        return Ok(0);
    };
    let vars = db.list_env_vars(&profile.id)?;
    // dotenv가 **직접 만든** credential(provider="env")만 삭제 후보 — 사용자가 dotenv
    // profile에 수동으로 붙인 외부 credential은 참조가 사라져도 보존한다(codex High).
    let dotenv_owned: std::collections::HashSet<String> = db
        .list_credential_secret_records(secret::VERSIONED_SECRET_BUNDLE_SLOT_CEILING)?
        .into_iter()
        .filter(|record| record.meta.provider == "env")
        .map(|record| record.meta.id)
        .collect();
    let mut removed = 0usize;
    for var in &vars {
        let cleanup_target = if let EnvValue::Secret { credential_id } = &var.value
            && dotenv_owned.contains(credential_id)
        {
            match resolve_secret_slot(db, credential_id) {
                Ok(slot) => Some(slot),
                Err(error) => {
                    warn_secret_failure("remove_resolve", secret_error_code(&error));
                    None
                }
            }
        } else {
            None
        };
        db.delete_env_var(&profile.id, &var.key)?;
        if let Some(target) = cleanup_target
            && let Err(error) = retire_dotenv_credential(db, secret_store, &target)
        {
            warn_secret_failure("remove_cleanup", secret_error_code(&error));
        }
        removed += 1;
    }
    db.delete_env_profile(&profile.id)?;
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
    read_dotenv_merged_bounded(root)
}

/// workspace 루트의 dotenv 파일들(`.env` → `.env.local` 병합)을 dotenv profile로
/// 동기화한다. 파일이 하나도 없으면 None.
/// secret 저장이 하나라도 실패하면 그 키만 건너뛰고 계속한다(best-effort).
pub fn sync_workspace_dotenv(
    db: &Db,
    secret_store: &dyn secret::SecretStore,
    redaction: &secret::RedactionService,
    workspace_id: &str,
    root: &Path,
) -> anyhow::Result<Option<DotenvSyncReport>> {
    let Some(parsed) = read_merged_dotenv(root)? else {
        return Ok(None); // dotenv 파일 없음 — 기존 profile은 보존
    };

    // dotenv profile 찾기/생성.
    let profile_id = match db
        .list_env_profiles(workspace_id)?
        .into_iter()
        .find(|p| p.kind == DOTENV_PROFILE_KIND)
    {
        Some(p) => p.id,
        None => db.insert_env_profile(workspace_id, DOTENV_PROFILE_NAME, DOTENV_PROFILE_KIND)?,
    };

    let existing = db.list_env_vars(&profile_id)?;
    let dotenv_owned: std::collections::HashSet<String> = db
        .list_credential_secret_records(secret::VERSIONED_SECRET_BUNDLE_SLOT_CEILING)?
        .into_iter()
        .filter(|record| record.meta.provider == "env")
        .map(|record| record.meta.id)
        .collect();
    let mut report = DotenvSyncReport::default();

    for (key, value) in &parsed {
        let current = existing.iter().find(|v| &v.key == key);
        // secret 판정은 **storage의 검증과 일치**시켜야 한다 — storage가 Plain으로 거부하는
        // 키(DATABASE_URL/DB_URL/*_TOKEN 등)나 값을 우리가 Plain으로 저장하려다 upsert가
        // 실패해 동기화 전체가 중단됐다(2026-07-08 실측). 우리 휴리스틱(is_secret_key)에
        // 더해 storage가 거부하면 secret으로 저장한다.
        let needs_secret = is_secret_key(key)
            || Db::validate_env_var_for_persistence(key, &EnvValue::Plain(value.clone())).is_err();
        if needs_secret {
            let secret = secret::SecretString::new(value.clone());
            // This operation owns only a bounded rotating redaction lease. Long-lived consumers
            // acquire their own execution lease when they resolve the physical slot; storing every
            // rotated dotenv value as a permanent corpus entry would make RSS grow over time.
            let _redaction_lease = redaction.acquire_rotating(&secret).ok();

            let (credential_id, newly_created) = match current.map(|v| &v.value) {
                Some(EnvValue::Secret { credential_id })
                    if dotenv_owned.contains(credential_id) =>
                {
                    match rotate_dotenv_credential(db, secret_store, credential_id, &secret) {
                        Ok(false) => continue,
                        Ok(true) => (credential_id.clone(), None),
                        Err(error) => {
                            warn_secret_failure("rotate", secret_error_code(&error));
                            continue;
                        }
                    }
                }
                _ => match create_dotenv_credential(db, secret_store, workspace_id, key, &secret) {
                    Ok(created) => (created.logical.as_str().to_owned(), Some(created)),
                    Err(error) => {
                        warn_secret_failure("create", secret_error_code(&error));
                        continue;
                    }
                },
            };
            if db
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
                    && let Err(error) = retire_dotenv_credential(db, secret_store, created)
                {
                    warn_secret_failure("bind_rollback", secret_error_code(&error));
                }
                return Err(static_secret_error(ERROR_SECRET_ENV_BIND));
            }
            report.upserted += 1;
        } else {
            // plain: 값이 같으면 write 생략 (DB churn 방지).
            if matches!(current.map(|v| &v.value), Some(EnvValue::Plain(v)) if v == value) {
                continue;
            }
            let replaced_secret = match current.map(|v| &v.value) {
                Some(EnvValue::Secret { credential_id })
                    if dotenv_owned.contains(credential_id) =>
                {
                    match resolve_secret_slot(db, credential_id) {
                        Ok(slot) => Some(slot),
                        Err(error) => {
                            warn_secret_failure("plain_replace_resolve", secret_error_code(&error));
                            None
                        }
                    }
                }
                _ => None,
            };
            db.upsert_env_var(&profile_id, key, &EnvValue::Plain(value.clone()))?;
            if let Some(target) = replaced_secret
                && let Err(error) = retire_dotenv_credential(db, secret_store, &target)
            {
                warn_secret_failure("plain_replace_cleanup", secret_error_code(&error));
            }
            report.upserted += 1;
        }
    }

    // 병합 결과에서 사라진 키 제거 (+ 이 profile 전용 credential 정리).
    for var in &existing {
        if parsed.iter().any(|(k, _)| k == &var.key) {
            continue;
        }
        let cleanup_target = if let EnvValue::Secret { credential_id } = &var.value
            && dotenv_owned.contains(credential_id)
        {
            match resolve_secret_slot(db, credential_id) {
                Ok(slot) => Some(slot),
                Err(error) => {
                    warn_secret_failure("prune_resolve", secret_error_code(&error));
                    None
                }
            }
        } else {
            None
        };
        db.delete_env_var(&profile_id, &var.key)?;
        if let Some(target) = cleanup_target
            && let Err(error) = retire_dotenv_credential(db, secret_store, &target)
        {
            warn_secret_failure("prune_cleanup", secret_error_code(&error));
        }
        report.removed += 1;
    }
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            &crate::storage::CredentialMeta {
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
        let db = Db::open(&dir.join("test.db")).unwrap();
        let marker = "hostile-logical-coordinate-marker";
        db.insert_credential(&crate::storage::CredentialMeta {
            id: marker.to_owned(),
            provider: "legacy".to_owned(),
            label: "fixture".to_owned(),
            credential_kind: "api_key".to_owned(),
            masked_hint: None,
            workspace_id: None,
        })
        .unwrap();

        let error = match resolve_secret_slot(&db, marker) {
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
        let db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        // 1차: plain + secret 생성
        std::fs::write(dir.join(".env"), "PORT=3000\nAPI_KEY=sk-123\n").unwrap();
        let report = sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
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
        let report = sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
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
        let report = sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
            .unwrap()
            .unwrap();
        assert_eq!((report.upserted, report.removed), (0, 0));
        assert_eq!(physical_slot_for(&db, credential_id), second_slot);

        // 3차: 키 삭제 → profile 반영, credential/keyring/ledger 정리.
        std::fs::write(dir.join(".env"), "PORT=4000\n").unwrap();
        let report = sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
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
            sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
                .unwrap()
                .is_none()
        );
        assert_eq!(db.list_env_vars(&profile.id).unwrap().len(), 1);
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
        let db = Db::open(&dir.join("test.db")).unwrap();
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
            sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir).unwrap();
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
        let db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        std::fs::write(dir.join(".env"), "API_KEY=sk-delete-failure\n").unwrap();
        sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir).unwrap();
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
        let report = sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
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
        sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir).unwrap();
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
        let db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        // .env + .env.local — 겹치는 PORT는 .env.local이 이긴다.
        std::fs::write(dir.join(".env"), "PORT=3000\nFOO=a\n").unwrap();
        std::fs::write(dir.join(".env.local"), "PORT=5000\nBAR=b\n").unwrap();
        let report = sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
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
        let report = sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
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
        let db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        std::fs::write(dir.join(".env.local"), "PORT=7000\n").unwrap();
        let report = sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
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
        let db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore::new();
        let redaction = secret::RedactionService::new();
        let ws = db.create_workspace("test").unwrap();

        std::fs::write(dir.join(".env"), "PORT=3000\n").unwrap();
        std::fs::write(dir.join(".env.local"), "API_KEY=sk-local\n").unwrap();
        sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir).unwrap();
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
        assert!(sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir).is_err());
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
            sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
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
        for k in ["NODE_ENV", "PORT", "LOG_LEVEL"] {
            assert!(!is_secret_key(k), "{k}는 plain이어야 함");
        }
    }
}
