//! Exact-bound authorization issuer for downstream crate tests only.
//!
//! This module is absent from normal production builds. It deliberately exposes no constructor
//! for grants or audit operations: tests must pass through the real migration, validator,
//! redaction, durable preflight, proof binding, and outcome transition.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rusqlite::Connection;

use crate::{
    AuditLifecycle, AuthorizationOutcome, AuthorizationPlan, AuthorizationPreflight,
    MIGRATION_AUDIT_LIFECYCLE, MIGRATION_AUTHORIZATION_OWNERS, MIGRATION_SQL,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TestAuthorizationCounters {
    pub preflights: u64,
    pub completions: u64,
}

/// Send-safe in-memory ledger used by connector-service tests without depending on storage.
pub struct InMemoryAuthorizationLedger {
    conn: Mutex<Connection>,
    scope: String,
    run_id: String,
    preflights: AtomicU64,
    completions: AtomicU64,
    fail_next_preflight: AtomicBool,
    completion_failures_remaining: AtomicU64,
}

impl InMemoryAuthorizationLedger {
    pub fn new(scope: &str) -> anyhow::Result<Self> {
        crate::authorization_scope_lock_key(scope)?;
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(MIGRATION_SQL)?;
        conn.execute_batch(MIGRATION_AUDIT_LIFECYCLE)?;
        conn.execute_batch(MIGRATION_AUTHORIZATION_OWNERS)?;
        Ok(Self {
            conn: Mutex::new(conn),
            scope: scope.to_owned(),
            run_id: uuid::Uuid::new_v4().to_string(),
            preflights: AtomicU64::new(0),
            completions: AtomicU64::new(0),
            fail_next_preflight: AtomicBool::new(false),
            completion_failures_remaining: AtomicU64::new(0),
        })
    }

    pub fn preflight(
        &self,
        plan: AuthorizationPlan,
        input_json: &[u8],
    ) -> anyhow::Result<AuthorizationPreflight> {
        if self.fail_next_preflight.swap(false, Ordering::SeqCst) {
            anyhow::bail!("injected authorization preflight failure");
        }
        let input_json = std::str::from_utf8(input_json)
            .map_err(|_| anyhow::anyhow!("tool input is not UTF-8 JSON"))?;
        let conn = self.conn.lock().expect("test authorization ledger lock");
        let preflight = crate::prepare_owned_authorization_preflight(
            &conn,
            plan,
            input_json,
            &secret::RedactionService::new(),
            &self.scope,
            &self.run_id,
        )?;
        self.preflights.fetch_add(1, Ordering::SeqCst);
        Ok(preflight)
    }

    pub fn complete(
        &self,
        operation_id: &str,
        outcome: AuthorizationOutcome,
    ) -> anyhow::Result<()> {
        if self
            .completion_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            anyhow::bail!("injected authorization completion failure");
        }
        let conn = self.conn.lock().expect("test authorization ledger lock");
        crate::complete_authorization_operation(
            &conn,
            &self.scope,
            &self.run_id,
            operation_id,
            outcome,
        )?;
        self.completions.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    pub fn lifecycle(&self, operation_id: &str) -> anyhow::Result<Option<AuditLifecycle>> {
        let conn = self.conn.lock().expect("test authorization ledger lock");
        crate::audit_lifecycle(&conn, operation_id)
    }

    pub fn counters(&self) -> TestAuthorizationCounters {
        TestAuthorizationCounters {
            preflights: self.preflights.load(Ordering::SeqCst),
            completions: self.completions.load(Ordering::SeqCst),
        }
    }

    pub fn fail_next_preflight(&self) {
        self.fail_next_preflight.store(true, Ordering::SeqCst);
    }

    pub fn fail_next_completion(&self) {
        self.fail_completions(1);
    }

    pub fn fail_completions(&self, count: u64) {
        self.completion_failures_remaining
            .store(count, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for InMemoryAuthorizationLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryAuthorizationLedger")
            .field("state", &"test-only")
            .finish()
    }
}
