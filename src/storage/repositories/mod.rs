pub mod audit_logs;
pub mod channels;
pub mod cluster;
pub mod connectors;
pub mod helpers;
pub mod packages;
pub mod trace_dlq;
pub mod traces;
pub(crate) mod versioned;
pub mod workflows;

use std::sync::Arc;

/// The repository set backing `AppState` and the background tasks, all
/// constructed from the same startup pool.
///
/// Also the `repos` group on `AppStateInner` (R26): bootstrap builds one and
/// moves it into state wholesale, so the state's repository set and the one
/// handed to background tasks can never drift apart.
pub struct Repositories {
    pub workflows: Arc<dyn workflows::WorkflowRepository>,
    pub channels: Arc<dyn channels::ChannelRepository>,
    pub connectors: Arc<dyn connectors::ConnectorRepository>,
    pub traces: Arc<dyn traces::TraceRepository>,
    pub audit_logs: Arc<dyn audit_logs::AuditLogRepository>,
    pub trace_dlq: Arc<dyn trace_dlq::TraceDlqRepository>,
    pub packages: Arc<dyn packages::PackageRepository>,
}

impl Repositories {
    /// Create repositories. `storage` supplies the optional at-rest cipher
    /// for connector configs (H3) — validated at config load, so a bad key
    /// never reaches this point.
    pub fn new(
        pool: &crate::storage::DbPool,
        storage: &crate::config::StorageConfig,
    ) -> Result<Self, crate::errors::OrionError> {
        let cipher = if storage.connector_encryption_key.is_empty() {
            None
        } else {
            Some(Arc::new(
                crate::storage::config_encryption::ConfigCipher::from_hex(
                    &storage.connector_encryption_key,
                )?,
            ))
        };
        Ok(Self {
            workflows: Arc::new(workflows::SqlWorkflowRepository::new(pool.clone())),
            channels: Arc::new(channels::SqlChannelRepository::new(pool.clone())),
            connectors: Arc::new(connectors::SqlConnectorRepository::with_cipher(
                pool.clone(),
                cipher,
            )),
            traces: Arc::new(traces::SqlTraceRepository::new(pool.clone())),
            audit_logs: Arc::new(audit_logs::SqlAuditLogRepository::new(pool.clone())),
            trace_dlq: Arc::new(trace_dlq::SqlTraceDlqRepository::new(pool.clone())),
            packages: Arc::new(packages::SqlPackageRepository::new(pool.clone())),
        })
    }
}
