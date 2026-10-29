use serde::{Deserialize, Serialize};

use super::CacheOperationGates;

/// Allowed cache backend values.
pub const VALID_CACHE_BACKENDS: &[&str] = &["memory"];

/// Cache connector: a named, per-node in-memory store. `default_ttl_secs`,
/// `max_connections`, `auth` and `retry` were accepted here until 1.0 and
/// never read (proposal F22): TTL comes from each `cache_write` task.
///
/// `url` and `allow_private_urls` described the Redis backend, which is no
/// longer in Community. Stored rows carrying them still parse (connector
/// configs do not deny unknown fields) and the keys are ignored; validation
/// refuses a non-null `url`, and a `backend` other than `memory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheConnectorConfig {
    /// Cache backend. `"memory"` is the only one. Required — no default, so
    /// a config written for another backend is refused rather than quietly
    /// becoming a per-node store.
    pub backend: String,
    /// Which operations workflows may run through this connector.
    #[serde(default)]
    pub operations: CacheOperationGates,
}

impl CacheConnectorConfig {
    /// Why this build cannot serve `backend`, naming the backends it can, or
    /// `None` when it can. One sentence for the authoring refusal, the
    /// registry load and the pool, so a stored `redis` row reads the same
    /// wherever it is met.
    pub fn unsupported_backend(&self) -> Option<String> {
        (!VALID_CACHE_BACKENDS.contains(&self.backend.as_str())).then(|| {
            format!(
                "Invalid cache backend '{}'. Must be one of: {}",
                self.backend,
                VALID_CACHE_BACKENDS.join(", ")
            )
        })
    }
}
