use mongodb::Client;

use super::lru_cache::LruCache;
use crate::connector::DbConnectorConfig;
use crate::errors::OrionError;

pub struct MongoPoolCache {
    cache: LruCache<Client>,
}

impl MongoPoolCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            // F17: shut evicted clients down gracefully on a detached task
            // (waits for in-flight operations, then drops the connections).
            cache: LruCache::with_evict_handler(max_entries, "mongo_pool", |client: Client| {
                tokio::spawn(async move { client.shutdown().await });
            }),
        }
    }

    /// Resolve (and cache) the client for `connector_name`.
    ///
    /// `max_connections` and `connect_timeout_ms` are honoured here for the
    /// same reason the SQL pool honours them (`pool_cache.rs`): without the
    /// timeout, an unreachable Mongo host waits on the driver's 30 s
    /// server-selection default while the caller's own deadline is typically
    /// far shorter, so the request stalls instead of failing. Both were
    /// accepted and ignored until 1.0 (proposal F22) — the same two fields the
    /// SQL path applied, on the same struct.
    pub async fn get_client(
        &self,
        connector_name: &str,
        config: &DbConnectorConfig,
    ) -> Result<Client, OrionError> {
        let conn_str = config.connection_string.clone();
        let max_conns = config.max_connections;
        let connect_timeout = config.connect_timeout_ms;
        let allow_private = config.allow_private_urls;
        let name = connector_name.to_string();

        self.cache
            .get_or_create(connector_name, || async move {
                let mut opts = mongodb::options::ClientOptions::parse(&conn_str)
                    .await
                    .map_err(|e| OrionError::InternalSource {
                        context: format!(
                            "Invalid MongoDB connection string for '{connector_name}'"
                        ),
                        source: Box::new(e),
                    })?;

                // S6: check the addresses the driver actually resolved. A
                // replica-set URI names several hosts and is not parseable as
                // a single URL, and `mongodb+srv://` has no hosts at all until
                // the SRV record is looked up — which `parse` just did. This
                // is the only place the real target list exists.
                let hosts: Vec<(String, Option<u16>)> = opts
                    .hosts
                    .iter()
                    .filter_map(|addr| match addr {
                        mongodb::options::ServerAddress::Tcp { host, port } => {
                            Some((host.clone(), *port))
                        }
                        // A Unix socket has no address to judge; it is also
                        // not reachable from a workflow-authored hostname.
                        _ => None,
                    })
                    .collect();
                crate::validation::check_mongo_hosts(&name, &hosts, allow_private).await?;
                if let Some(max) = max_conns {
                    opts.max_pool_size = Some(max);
                }
                if let Some(ms) = connect_timeout {
                    let d = std::time::Duration::from_millis(ms);
                    opts.connect_timeout = Some(d);
                    // Connecting is only half of it: with a reachable host but
                    // no usable primary the driver blocks in server selection,
                    // which has its own (30 s) default.
                    opts.server_selection_timeout = Some(d);
                }
                Client::with_options(opts).map_err(|e| OrionError::InternalSource {
                    context: format!("Failed to connect to MongoDB '{connector_name}'"),
                    source: Box::new(e),
                })
            })
            .await
    }

    pub async fn evict(&self, connector_name: &str) {
        self.cache.evict(connector_name).await;
    }

    pub async fn evict_all(&self) {
        self.cache.evict_all().await;
    }
}

impl Default for MongoPoolCache {
    fn default() -> Self {
        Self::new(100)
    }
}
