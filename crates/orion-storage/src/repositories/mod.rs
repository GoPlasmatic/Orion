pub mod audit_logs;
pub mod cluster;
pub mod cron;
pub mod helpers;
pub mod job_lease;
pub mod oauth_state;
pub mod shared_state;
pub mod trace_dlq;
pub mod traces;

use std::sync::Arc;

/// The repository set backing `AppState` and the background tasks, all
/// constructed from the same startup pool.
///
/// Also the `repos` group on `AppStateInner` (R26): bootstrap builds one and
/// moves it into state wholesale, so the state's repository set and the one
/// handed to background tasks can never drift apart.
///
/// What a node serves is not here: definitions and connections live in the
/// v2 deployment store (`extensions::deployment_store`).
pub struct Repositories {
    pub traces: Arc<dyn traces::TraceRepository>,
    pub audit_logs: Arc<dyn audit_logs::AuditLogRepository>,
    pub trace_dlq: Arc<dyn trace_dlq::TraceDlqRepository>,
    /// The cron ledger: cursors, occurrences and singletons.
    pub cron: Arc<dyn cron::CronRepository>,
    /// Managed-OAuth2 token state, per connector name.
    pub oauth_state: Arc<dyn oauth_state::OAuthStateRepository>,
}

impl Repositories {
    /// Create repositories. `storage` supplies the optional at-rest cipher
    /// for OAuth2 token state (H3) — validated at config load, so a bad key
    /// never reaches this point.
    pub fn new(
        pool: &crate::DbPool,
        storage: &orion_common::config::StorageConfig,
    ) -> Result<Self, orion_common::errors::OrionError> {
        let cipher = if storage.connector_encryption_key.is_empty() {
            None
        } else {
            Some(Arc::new(
                orion_common::config_encryption::ConfigCipher::from_hex(
                    &storage.connector_encryption_key,
                )?,
            ))
        };
        Ok(Self {
            traces: Arc::new(traces::SqlTraceRepository::new(pool.clone())),
            audit_logs: Arc::new(audit_logs::SqlAuditLogRepository::new(pool.clone())),
            trace_dlq: Arc::new(trace_dlq::SqlTraceDlqRepository::new(pool.clone())),
            cron: Arc::new(cron::SqlCronRepository::new(pool.clone())),
            oauth_state: Arc::new(oauth_state::SqlOAuthStateRepository::new(
                pool.clone(),
                cipher,
            )),
        })
    }
}
