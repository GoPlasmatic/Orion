//! A [`ConnectionResolver`] over one governed connection revision, held
//! outside any connector call.
//!
//! A connector call reaches its connection through the attempt's
//! [`super::context::Invocation`]. A channel driver's listener (a Kafka
//! consumer) needs the same view for as long as it runs: the revision its
//! channel requires in the deployment, its public configuration, and its
//! secret locators resolved on demand. Both resolve through the functions here,
//! so the two can never disagree about what a connection says.
//!
//! Secrets are resolved per call and never cached, exactly as for an attempt:
//! a rotated `env://` or `vault://` value is what the next call sees.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::deployment_store::{ConnectionRevisionRow, ImmutableDeploymentStore};
use super::{
    ConnectionResolver, ConnectorError, EffectiveConnectionIdentity, Revision, SecretValue,
};

/// One connection revision the deployment pins, as a resolver.
#[derive(Debug, Clone)]
pub struct GovernedConnection {
    row: Arc<ConnectionRevisionRow>,
    identity: EffectiveConnectionIdentity,
    /// The provider's id, for error text.
    provider: String,
}

impl GovernedConnection {
    /// The exact revision `connection_id`@`revision` in `store`, or `None`
    /// when the node does not hold it.
    pub fn from_store(
        store: &ImmutableDeploymentStore,
        connection_id: &str,
        revision: Revision,
    ) -> Option<Self> {
        let row = store.get_connection(connection_id, revision)?;
        let identity = row.effective_identity();
        let provider = store.provider_ref(&row.provider_digest).map_or_else(
            || connection_id.to_string(),
            |p| p.provider_id.as_str().to_string(),
        );
        Some(Self {
            row,
            identity,
            provider,
        })
    }

    /// The revision's identity: provider digest, connection, revision and
    /// secret hash — what a provider keys its pools on.
    pub fn identity(&self) -> &EffectiveConnectionIdentity {
        &self.identity
    }
}

#[async_trait]
impl ConnectionResolver for GovernedConnection {
    fn public_config(&self) -> Option<&Value> {
        Some(&self.row.public_config)
    }

    async fn resolve_secret(&self, path: &str) -> Result<Option<SecretValue>, ConnectorError> {
        resolve_secret(&self.row, path, &self.identity, &self.provider).await
    }

    async fn resolve_merged_config(&self) -> Result<Option<Value>, ConnectorError> {
        merged_config(&self.row, &self.identity, &self.provider)
            .await
            .map(Some)
    }
}

/// `path`'s secret locator in `row`, resolved, or `None` when it names none.
pub(super) async fn resolve_secret(
    row: &ConnectionRevisionRow,
    path: &str,
    identity: &EffectiveConnectionIdentity,
    provider: &str,
) -> Result<Option<SecretValue>, ConnectorError> {
    super::foundation::governed_secret(row, path, identity, provider)
        .await
        .map(|opt| opt.map(SecretValue::new))
}

/// `row`'s public configuration with every secret locator resolved in place.
pub(super) async fn merged_config(
    row: &ConnectionRevisionRow,
    identity: &EffectiveConnectionIdentity,
    provider: &str,
) -> Result<Value, ConnectorError> {
    let mut resolved = BTreeMap::new();
    for path in row.secret_locators.keys() {
        if let Some(secret) =
            super::foundation::governed_secret(row, path, identity, provider).await?
        {
            resolved.insert(path.clone(), secret);
        }
    }
    Ok(super::deployment_store::place_locators(
        &row.public_config,
        &resolved,
    ))
}
