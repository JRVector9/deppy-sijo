//! 프로젝트 루트의 `.env`를 설정 › 환경(env profile)으로 자동 동기화한다 (2026-07-07).
//!
//! - workspace 활성화 시 루트 `.env`를 파싱해 kind=`dotenv` profile로 upsert한다.
//!   secret으로 보이는 키(API/SECRET/TOKEN/…)는 값이 DB가 아닌 **OS keyring**(credential)에
//!   저장되고, 나머지는 plain으로 저장된다. `.env`에서 사라진 키는 profile에서도 지운다.
//! - `.env`가 source of truth — dotenv profile의 해당 키를 UI에서 고쳐도 다음 동기화가
//!   `.env` 값으로 되돌린다(다른 profile은 건드리지 않음).
//! - `.env` 파일이 없으면 아무것도 만들지 않고, 기존 dotenv profile은 그대로 둔다
//!   (일시적 체크아웃 차이로 저장된 환경이 사라지지 않게).

use std::path::Path;

use crate::env::EnvValue;
use crate::storage::Db;

/// dotenv 자동 profile의 kind. 환경 UI에는 일반 profile처럼 보인다.
pub const DOTENV_PROFILE_KIND: &str = "dotenv";
/// dotenv 자동 profile 이름.
pub const DOTENV_PROFILE_NAME: &str = ".env";

/// `.env` 한 파일을 파싱한다 — `KEY=VALUE`, `export KEY=VALUE`, 주석(#)/빈 줄 무시,
/// 양끝 따옴표('...', "...") 제거. 잘못된 줄은 건너뛴다(엄격 실패 없음 — 사용자 파일).
pub fn parse_dotenv(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        {
            continue;
        }
        let value = value.trim();
        // 따옴표 값은 닫는 따옴표까지가 값 — 그 뒤(후행 주석 등)는 버린다.
        // `FOO="bar" # comment`가 `"bar"`로 저장되던 것 수정(codex 리뷰).
        let value = if let Some(rest) = value.strip_prefix('"') {
            rest.split_once('"').map_or(rest, |(v, _)| v)
        } else if let Some(rest) = value.strip_prefix('\'') {
            rest.split_once('\'').map_or(rest, |(v, _)| v)
        } else if let Some((v, _comment)) = value.split_once(" #") {
            v.trim_end()
        } else {
            value
        };
        out.push((key.to_owned(), value.to_owned()));
    }
    out
}

/// 키 이름으로 secret 여부를 판별한다 — 보수적으로 넓게 잡는다(secret이 DB 평문으로
/// 남는 것보다 plain이 keyring에 들어가는 쪽이 안전).
pub fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "SECRET",
        "TOKEN",
        "KEY",
        "PASSWORD",
        "PASSWD",
        "PWD",
        "CREDENTIAL",
        "AUTH",
        "PRIVATE",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
}

/// 동기화 결과 요약 (로그/알림용).
#[derive(Debug, Default, PartialEq)]
pub struct DotenvSyncReport {
    pub upserted: usize,
    pub removed: usize,
}

/// workspace 루트의 `.env`를 dotenv profile로 동기화한다. `.env`가 없으면 None.
/// secret 저장이 하나라도 실패하면 그 키만 건너뛰고 계속한다(best-effort).
pub fn sync_workspace_dotenv(
    db: &Db,
    secret_store: &dyn secret::SecretStore,
    redaction: &secret::RedactionService,
    workspace_id: &str,
    root: &Path,
) -> anyhow::Result<Option<DotenvSyncReport>> {
    let env_path = root.join(".env");
    let Ok(content) = std::fs::read_to_string(&env_path) else {
        return Ok(None); // .env 없음 — 기존 profile은 보존
    };
    let parsed = parse_dotenv(&content);

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
        if is_secret_key(key) {
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

    // `.env`에서 사라진 키 제거 (+ 이 profile 전용 credential 정리).
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
    fn is_secret_key는_민감_키를_넓게_잡는다() {
        for k in [
            "API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "DB_PASSWORD",
            "AUTH_DOMAIN",
        ] {
            assert!(is_secret_key(k), "{k}는 secret이어야 함");
        }
        for k in ["DATABASE_URL", "NODE_ENV", "PORT", "LOG_LEVEL"] {
            assert!(!is_secret_key(k), "{k}는 plain이어야 함");
        }
    }
}
