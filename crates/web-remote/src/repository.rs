//! App-owned persistence port for the optional web-remote server.
//!
//! The web server owns network/thread lifecycles, but it must not construct or expose the
//! application's concrete storage handle. The composition root supplies one implementation when
//! the server is enabled; disabled web-remote therefore owns no repository, thread, socket, or
//! polling lifecycle.

/// Maximum pending approval rows a web-remote snapshot may materialize.
pub const PENDING_APPROVAL_LIMIT: usize = 256;
/// Maximum persisted browser push subscriptions.
pub const PUSH_SUBSCRIPTION_LIMIT: usize = 8;

/// Storage-neutral pending approval projection. `arguments_preview` must already be sanitized by
/// the authorization/audit layer; raw tool arguments are never part of this port.
pub struct PendingApprovalRecord {
    pub id: String,
    pub server_id: String,
    pub tool_name: String,
    pub arguments_preview: String,
    pub created_at: i64,
    pub pane_id: Option<String>,
}

/// Storage-neutral web-push target. Browser auth material is deliberately non-Clone and omitted
/// from Debug so repository results cannot be duplicated or logged accidentally.
pub struct PushSubscriptionRecord {
    endpoint: String,
    p256dh: String,
    auth: String,
}

impl PushSubscriptionRecord {
    pub fn new(endpoint: String, p256dh: String, auth: String) -> Self {
        Self {
            endpoint,
            p256dh,
            auth,
        }
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn p256dh(&self) -> &str {
        &self.p256dh
    }

    pub(crate) fn auth(&self) -> &str {
        &self.auth
    }
}

impl std::fmt::Debug for PushSubscriptionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushSubscriptionRecord")
            .field("endpoint", &"REDACTED")
            .field("p256dh", &"REDACTED")
            .field("auth", &"REDACTED")
            .finish()
    }
}

/// Result of one bounded subscription upsert. The repository serializes the limit check and
/// write, preventing two concurrent browser registrations from exceeding the configured cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionUpsert {
    Stored { total: usize },
    LimitReached,
}

/// Persistence capability supplied by the application composition root.
///
/// Implementations own and serialize their concrete connection. Calls occur only on the optional
/// dashboard/push workers, never on the GUI render thread. Implementations must honor `limit`
/// before materializing more rows than requested.
pub trait WebRemoteRepository: Send + Sync {
    fn list_pending_approvals(&self, limit: usize) -> anyhow::Result<Vec<PendingApprovalRecord>>;

    fn resolve_approval(
        &self,
        id: &str,
        allowed: bool,
        remember: bool,
        resolved_at: i64,
    ) -> anyhow::Result<()>;

    fn web_push_subscription_count(&self) -> anyhow::Result<usize>;

    fn upsert_web_push_subscription(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        created_at: i64,
        limit: usize,
    ) -> anyhow::Result<SubscriptionUpsert>;

    fn list_web_push_subscriptions(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<PushSubscriptionRecord>>;

    fn touch_web_push_subscription(&self, endpoint: &str, last_ok_at: i64) -> anyhow::Result<()>;

    /// Deletes one endpoint and returns the remaining bounded subscription count.
    fn delete_web_push_subscription(&self, endpoint: &str) -> anyhow::Result<usize>;
}

#[cfg(test)]
pub(crate) struct StorageTestRepository {
    db: std::sync::Mutex<storage::Db>,
}

#[cfg(test)]
impl StorageTestRepository {
    pub(crate) fn open(path: &std::path::Path) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            db: std::sync::Mutex::new(storage::Db::open(path).expect("test repository open")),
        })
    }
}

#[cfg(test)]
impl WebRemoteRepository for StorageTestRepository {
    fn list_pending_approvals(&self, limit: usize) -> anyhow::Result<Vec<PendingApprovalRecord>> {
        let page = self
            .db
            .lock()
            .expect("test repository lock")
            .list_pending_approvals_bounded(limit)?;
        Ok(page
            .rows
            .into_iter()
            .map(|row| PendingApprovalRecord {
                id: row.id,
                server_id: row.server_id,
                tool_name: row.tool_name,
                arguments_preview: row.arguments_preview,
                created_at: row.created_at,
                pane_id: row.pane_id,
            })
            .collect())
    }

    fn resolve_approval(
        &self,
        id: &str,
        allowed: bool,
        remember: bool,
        resolved_at: i64,
    ) -> anyhow::Result<()> {
        self.db
            .lock()
            .expect("test repository lock")
            .resolve_approval(id, allowed, remember, resolved_at)
    }

    fn web_push_subscription_count(&self) -> anyhow::Result<usize> {
        let count = self
            .db
            .lock()
            .expect("test repository lock")
            .count_web_push_subscriptions()?;
        usize::try_from(count).map_err(Into::into)
    }

    fn upsert_web_push_subscription(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        created_at: i64,
        limit: usize,
    ) -> anyhow::Result<SubscriptionUpsert> {
        let db = self.db.lock().expect("test repository lock");
        let existing = db.list_web_push_subscriptions()?;
        if existing.len() >= limit && !existing.iter().any(|row| row.endpoint == endpoint) {
            return Ok(SubscriptionUpsert::LimitReached);
        }
        db.upsert_web_push_subscription(endpoint, p256dh, auth, created_at)?;
        let total = usize::try_from(db.count_web_push_subscriptions()?)?;
        Ok(SubscriptionUpsert::Stored { total })
    }

    fn list_web_push_subscriptions(
        &self,
        limit: usize,
    ) -> anyhow::Result<Vec<PushSubscriptionRecord>> {
        let rows = self
            .db
            .lock()
            .expect("test repository lock")
            .list_web_push_subscriptions()?;
        anyhow::ensure!(rows.len() <= limit, "web_push_subscription_limit_exceeded");
        Ok(rows
            .into_iter()
            .map(|row| PushSubscriptionRecord::new(row.endpoint, row.p256dh, row.auth))
            .collect())
    }

    fn touch_web_push_subscription(&self, endpoint: &str, last_ok_at: i64) -> anyhow::Result<()> {
        self.db
            .lock()
            .expect("test repository lock")
            .touch_web_push_subscription(endpoint, last_ok_at)
    }

    fn delete_web_push_subscription(&self, endpoint: &str) -> anyhow::Result<usize> {
        let db = self.db.lock().expect("test repository lock");
        db.delete_web_push_subscription(endpoint)?;
        Ok(usize::try_from(db.count_web_push_subscriptions()?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "deppy-web-remote-repository-{name}-{}-{}.sqlite3",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn push_subscription_debug는_모든_인증좌표를_숨긴다() {
        let record = PushSubscriptionRecord::new(
            "https://push.example/private-coordinate".to_owned(),
            "browser-public-key".to_owned(),
            "browser-auth-secret".to_owned(),
        );
        let debug = format!("{record:?}");
        assert!(!debug.contains("private-coordinate"), "{debug}");
        assert!(!debug.contains("browser-public-key"), "{debug}");
        assert!(!debug.contains("browser-auth-secret"), "{debug}");
        assert_eq!(debug.matches("REDACTED").count(), 3, "{debug}");
    }

    #[test]
    fn storage_test_port는_구독상한을_원자적으로_적용한다() {
        let path = temp_db("subscription-limit");
        let repository = StorageTestRepository::open(&path);
        for index in 0..2 {
            assert_eq!(
                repository
                    .upsert_web_push_subscription(
                        &format!("https://push.example/{index}"),
                        "p256dh",
                        "auth",
                        1,
                        2,
                    )
                    .unwrap(),
                SubscriptionUpsert::Stored { total: index + 1 }
            );
        }
        assert_eq!(
            repository
                .upsert_web_push_subscription(
                    "https://push.example/overflow",
                    "p256dh",
                    "auth",
                    1,
                    2,
                )
                .unwrap(),
            SubscriptionUpsert::LimitReached
        );
        assert_eq!(repository.web_push_subscription_count().unwrap(), 2);

        drop(repository);
        let _ = std::fs::remove_file(path);
    }
}
