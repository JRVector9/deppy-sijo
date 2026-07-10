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

use std::path::Path;

use anyhow::Context;

use crate::env::EnvValue;
use crate::storage::Db;

/// dotenv 자동 profile의 kind. 환경 UI에는 일반 profile처럼 보인다.
pub const DOTENV_PROFILE_KIND: &str = "dotenv";
/// dotenv 자동 profile 이름.
pub const DOTENV_PROFILE_NAME: &str = ".env";
/// 스캔·병합할 dotenv 파일 이름(관례 순서 — 뒤 파일이 같은 키를 덮어씀).
pub const DOTENV_FILE_NAMES: [&str; 2] = [".env", ".env.local"];

pub use runtime::dotenv::{is_secret_key, parse_dotenv};

/// 동기화 결과 요약 (로그/알림용).
#[derive(Debug, Default, PartialEq)]
pub struct DotenvSyncReport {
    pub upserted: usize,
    pub removed: usize,
}

/// UI 편집을 `.env` 파일에 **라인 단위**로 반영한다(7·8번, 2026-07-10). 주석·순서 보존.
/// - 대상 파일: 키가 이미 있는 파일(.env.local 우선순위 역순으로 탐색), 없으면 `.env`
///   (파일이 없으면 생성). value=None이면 해당 라인 삭제.
/// - 반영 후 mtime 폴링/명시 sync가 DB를 따라 갱신한다(.env가 단일 진실).
pub fn write_env_var(root: &std::path::Path, key: &str, value: Option<&str>) -> anyhow::Result<()> {
    // 키가 존재하는 파일 찾기 — 병합 우선순위가 높은 파일(.env.local)부터.
    let mut target: Option<std::path::PathBuf> = None;
    for name in DOTENV_FILE_NAMES.iter().rev() {
        let path = root.join(name);
        if let Ok(content) = std::fs::read_to_string(&path)
            && parse_dotenv(&content).iter().any(|(k, _)| k == key)
        {
            target = Some(path);
            break;
        }
    }
    let path = target.unwrap_or_else(|| root.join(DOTENV_FILE_NAMES[0]));
    let content = std::fs::read_to_string(&path).unwrap_or_default();

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
    let existing = lines.iter().position(|l| matches_key(l));
    match (existing, value) {
        (Some(idx), Some(v)) => lines[idx] = render(v)?,
        (Some(idx), None) => {
            lines.remove(idx);
        }
        (None, Some(v)) => lines.push(render(v)?),
        (None, None) => return Ok(()), // 지울 것 없음
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    std::fs::write(&path, out).with_context(|| format!(".env 기록 실패: {}", path.display()))?;
    Ok(())
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
        .list_credentials()?
        .into_iter()
        .filter(|c| c.provider == "env")
        .map(|c| c.id)
        .collect();
    let mut removed = 0usize;
    for var in &vars {
        db.delete_env_var(&profile.id, &var.key)?;
        if let EnvValue::Secret { credential_id } = &var.value
            && dotenv_owned.contains(credential_id)
            && db
                .delete_credential_if_unused(credential_id)
                .unwrap_or(false)
        {
            let _ = secret_store.delete_secret(credential_id);
        }
        removed += 1;
    }
    db.delete_env_profile(&profile.id)?;
    tracing::info!(removed, "프로젝트 해제 — dotenv profile 정리");
    Ok(removed)
}

/// 루트의 dotenv 파일들(`DOTENV_FILE_NAMES` 순서)을 읽어 병합 파싱한다.
/// 같은 키는 뒤 파일 값이 이긴다(순서는 처음 등장 위치 유지). 읽은 파일이 없으면 None.
fn read_merged_dotenv(root: &Path) -> Option<Vec<(String, String)>> {
    let mut merged: Vec<(String, String)> = Vec::new();
    let mut found = false;
    for name in DOTENV_FILE_NAMES {
        let Ok(content) = std::fs::read_to_string(root.join(name)) else {
            continue;
        };
        found = true;
        for (key, value) in parse_dotenv(&content) {
            match merged.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 = value,
                None => merged.push((key, value)),
            }
        }
    }
    found.then_some(merged)
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
    let Some(parsed) = read_merged_dotenv(root) else {
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
            redaction.register(&secret);
            // 기존 secret var면 credential 재사용(keyring 값만 갱신), 아니면 새로 만든다.
            let credential_id = match current.map(|v| &v.value) {
                Some(EnvValue::Secret { credential_id }) => {
                    // 값이 같아도 keyring 덮어쓰기는 idempotent — 비교를 위해 읽는 것보다 싸다.
                    if secret_store.set_secret(credential_id, &secret).is_err() {
                        tracing::warn!(key, "dotenv secret keyring 갱신 실패 — 건너뜀");
                        continue;
                    }
                    credential_id.clone()
                }
                _ => {
                    let id = uuid::Uuid::new_v4().to_string();
                    if secret_store.set_secret(&id, &secret).is_err() {
                        tracing::warn!(key, "dotenv secret keyring 저장 실패 — 건너뜀");
                        continue;
                    }
                    let meta = crate::storage::CredentialMeta {
                        id: id.clone(),
                        provider: "env".to_owned(),
                        label: format!("{key} (.env)"),
                        credential_kind: "api_key".to_owned(),
                        masked_hint: Some(secret::masked_hint(value)),
                        // .env발 credential은 해당 프로젝트 소속(#2).
                        workspace_id: Some(workspace_id.to_owned()),
                    };
                    if let Err(e) = db.insert_credential(&meta) {
                        let _ = secret_store.delete_secret(&id);
                        tracing::warn!(key, "dotenv credential 저장 실패 — 건너뜀: {e:#}");
                        continue;
                    }
                    id
                }
            };
            db.upsert_env_var(&profile_id, key, &EnvValue::Secret { credential_id })?;
            report.upserted += 1;
        } else {
            // plain: 값이 같으면 write 생략 (DB churn 방지).
            if matches!(current.map(|v| &v.value), Some(EnvValue::Plain(v)) if v == value) {
                continue;
            }
            db.upsert_env_var(&profile_id, key, &EnvValue::Plain(value.clone()))?;
            report.upserted += 1;
        }
    }

    // 병합 결과에서 사라진 키 제거 (+ 이 profile 전용 credential 정리).
    for var in &existing {
        if parsed.iter().any(|(k, _)| k == &var.key) {
            continue;
        }
        db.delete_env_var(&profile_id, &var.key)?;
        if let EnvValue::Secret { credential_id } = &var.value {
            // 다른 곳에서 참조 중이면 남긴다(조건부 삭제). keyring은 삭제 성공 시에만 정리.
            if db
                .delete_credential_if_unused(credential_id)
                .unwrap_or(false)
            {
                let _ = secret_store.delete_secret(credential_id);
            }
        }
        report.removed += 1;
    }
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
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

    /// 테스트용 in-memory secret store.
    struct MemStore(std::sync::Mutex<std::collections::HashMap<String, String>>);
    impl secret::SecretStore for MemStore {
        fn set_secret(&self, id: &str, s: &secret::SecretString) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_owned(), s.expose().to_owned());
            Ok(())
        }
        fn get_secret(&self, id: &str) -> anyhow::Result<secret::SecretString> {
            self.0
                .lock()
                .unwrap()
                .get(id)
                .map(|v| secret::SecretString::new(v.clone()))
                .ok_or_else(|| anyhow::anyhow!("없음"))
        }
        fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
        fn has_secret(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.0.lock().unwrap().contains_key(id))
        }
    }

    #[test]
    fn sync는_생성_갱신_삭제를_수행하고_secret은_keyring으로_보낸다() {
        let dir = std::env::temp_dir().join(format!("deppy-dotenv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore(std::sync::Mutex::new(std::collections::HashMap::new()));
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
        assert_eq!(
            store.0.lock().unwrap().get(credential_id).unwrap(),
            "sk-123"
        );

        // 2차: 값 갱신 + 키 삭제 → profile 반영, credential/keyring 정리
        std::fs::write(dir.join(".env"), "PORT=4000\n").unwrap();
        let report = sync_workspace_dotenv(&db, &store, &redaction, &ws, &dir)
            .unwrap()
            .unwrap();
        assert_eq!((report.upserted, report.removed), (1, 1));
        let vars = db.list_env_vars(&profile.id).unwrap();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].value, EnvValue::Plain("4000".to_owned()));
        assert!(store.0.lock().unwrap().is_empty(), "keyring 정리됨");

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
    fn env_local이_env를_덮어쓰고_병합_기준으로_삭제한다() {
        let dir =
            std::env::temp_dir().join(format!("deppy-dotenv-merge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("test.db")).unwrap();
        let store = MemStore(std::sync::Mutex::new(std::collections::HashMap::new()));
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
        let store = MemStore(std::sync::Mutex::new(std::collections::HashMap::new()));
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
