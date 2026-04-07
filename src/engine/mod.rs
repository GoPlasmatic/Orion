pub mod functions;
pub mod utils;

use std::collections::HashMap;
use std::sync::Arc;

use dataflow_rs::engine::functions::AsyncFunctionHandler;

use tokio::sync::RwLock;

use crate::connector::ConnectorRegistry;
use crate::storage::models::{Channel, Workflow};
use crate::storage::repositories::workflows::{
    workflow_to_dataflow, workflow_to_dataflow_with_rollout,
};

/// Acquire the engine read lock with timing instrumentation.
///
/// Records lock wait time as a histogram metric and returns the cloned inner `Arc<Engine>`,
/// releasing the lock immediately.
pub async fn acquire_engine_read(
    lock: &RwLock<Arc<dataflow_rs::Engine>>,
) -> Arc<dataflow_rs::Engine> {
    let start = std::time::Instant::now();
    let guard = lock.read().await;
    crate::metrics::record_engine_lock_wait("read", start.elapsed().as_secs_f64());
    guard.clone()
}

/// Acquire the engine write lock with timing instrumentation.
///
/// Records lock wait time as a histogram metric.
pub async fn acquire_engine_write(
    lock: &RwLock<Arc<dataflow_rs::Engine>>,
) -> tokio::sync::RwLockWriteGuard<'_, Arc<dataflow_rs::Engine>> {
    let start = std::time::Instant::now();
    let guard = lock.write().await;
    crate::metrics::record_engine_lock_wait("write", start.elapsed().as_secs_f64());
    guard
}

/// Known function names supported by the engine.
pub const KNOWN_FUNCTIONS: &[&str] = &[
    "map",
    "validation",
    "validate",
    "parse_json",
    "parse_xml",
    "publish_json",
    "publish_xml",
    "filter",
    "log",
    "http_call",
    "publish_kafka",
    "db_read",
    "db_write",
    "cache_read",
    "cache_write",
    "mongo_read",
    "channel_call",
];

/// Function names that require a connector reference.
pub const CONNECTOR_FUNCTIONS: &[&str] = &[
    "http_call",
    "publish_kafka",
    "db_read",
    "db_write",
    "cache_read",
    "cache_write",
    "mongo_read",
];

/// Build the custom function handlers for the dataflow-rs engine.
///
/// Registers http_call, channel_call, and (when kafka feature is disabled) a
/// stub publish_kafka handler. Use [`register_kafka_publisher`] to register the
/// real Kafka-backed handler when the feature is enabled.
pub fn build_custom_functions(
    registry: Arc<ConnectorRegistry>,
    client: reqwest::Client,
    engine: Arc<tokio::sync::RwLock<Arc<dataflow_rs::Engine>>>,
    engine_config: &crate::config::EngineConfig,
) -> HashMap<String, Box<dyn AsyncFunctionHandler + Send + Sync>> {
    let mut fns: HashMap<String, Box<dyn AsyncFunctionHandler + Send + Sync>> = HashMap::new();

    fns.insert(
        "http_call".to_string(),
        Box::new(functions::http_call::HttpCallHandler {
            registry: registry.clone(),
            client: client.clone(),
        }),
    );

    fns.insert(
        "channel_call".to_string(),
        Box::new(functions::channel_call::ChannelCallHandler {
            engine,
            max_call_depth: engine_config.max_channel_call_depth,
            default_timeout_ms: engine_config.default_channel_call_timeout_ms,
        }),
    );

    // Register stub publish_kafka when kafka feature is not available
    #[cfg(not(feature = "kafka"))]
    fns.insert(
        "publish_kafka".to_string(),
        Box::new(functions::publish_kafka::PublishKafkaHandler {
            registry: registry.clone(),
        }),
    );

    // Register SQL database handlers (db_read, db_write)
    #[cfg(feature = "connectors-sql")]
    {
        let sql_pool_cache = Arc::new(crate::connector::pool_cache::SqlPoolCache::new());
        fns.insert(
            "db_read".to_string(),
            Box::new(functions::db_read::DbReadHandler {
                pool_cache: sql_pool_cache.clone(),
                registry: registry.clone(),
            }),
        );
        fns.insert(
            "db_write".to_string(),
            Box::new(functions::db_write::DbWriteHandler {
                pool_cache: sql_pool_cache,
                registry: registry.clone(),
            }),
        );
    }

    // Register Redis cache handlers (cache_read, cache_write)
    #[cfg(feature = "connectors-redis")]
    {
        let redis_pool_cache = Arc::new(crate::connector::redis_pool::RedisPoolCache::new());
        fns.insert(
            "cache_read".to_string(),
            Box::new(functions::cache_read::CacheReadHandler {
                pool_cache: redis_pool_cache.clone(),
                registry: registry.clone(),
            }),
        );
        fns.insert(
            "cache_write".to_string(),
            Box::new(functions::cache_write::CacheWriteHandler {
                pool_cache: redis_pool_cache,
                registry: registry.clone(),
            }),
        );
    }

    // Register MongoDB handler (mongo_read)
    #[cfg(feature = "connectors-mongodb")]
    {
        let mongo_pool_cache = Arc::new(crate::connector::mongo_pool::MongoPoolCache::new());
        fns.insert(
            "mongo_read".to_string(),
            Box::new(functions::mongo_read::MongoReadHandler {
                pool_cache: mongo_pool_cache,
                registry: registry.clone(),
            }),
        );
    }

    fns
}

/// Register the real Kafka-backed publish_kafka handler.
///
/// Replaces the stub handler (or adds the handler if not yet registered).
#[cfg(feature = "kafka")]
pub fn register_kafka_publisher(
    fns: &mut HashMap<String, Box<dyn AsyncFunctionHandler + Send + Sync>>,
    registry: Arc<ConnectorRegistry>,
    producer: Arc<crate::kafka::producer::KafkaProducer>,
) {
    fns.insert(
        "publish_kafka".to_string(),
        Box::new(functions::publish_kafka::PublishKafkaHandler { registry, producer }),
    );
}

/// Filter channels based on include/exclude glob patterns from [`ChannelLoadingConfig`].
///
/// - If `include` is non-empty, only channels matching at least one include pattern are kept.
/// - Channels matching any `exclude` pattern are removed (applied after include).
/// - Supports simple `*` wildcards (e.g., `internal-*`, `*-debug`).
pub fn filter_channels(
    channels: Vec<Channel>,
    config: &crate::config::ChannelLoadingConfig,
) -> Vec<Channel> {
    if config.include.is_empty() && config.exclude.is_empty() {
        return channels;
    }

    channels
        .into_iter()
        .filter(|ch| {
            // Include filter: if non-empty, channel must match at least one pattern
            if !config.include.is_empty() && !config.include.iter().any(|p| glob_match(p, &ch.name))
            {
                return false;
            }
            // Exclude filter: channel must not match any exclude pattern
            !config.exclude.iter().any(|p| glob_match(p, &ch.name))
        })
        .collect()
}

/// Simple glob matching supporting `*` wildcards.
fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        // No wildcard — exact match
        return pattern == name;
    }

    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if let Some(found) = name[pos..].find(part) {
            if i == 0 && found != 0 {
                // First segment must be a prefix match
                return false;
            }
            pos += found + part.len();
        } else {
            return false;
        }
    }

    // If pattern ends with *, remaining chars are fine. Otherwise name must be fully consumed.
    if pattern.ends_with('*') {
        true
    } else {
        pos == name.len()
    }
}

/// Convert active channels and their workflows to dataflow-rs workflows for the engine.
///
/// For each active channel, finds the associated workflow(s) and builds
/// dataflow-rs Workflow objects with the channel name injected as the channel field.
pub fn build_engine_workflows(
    channels: &[Channel],
    workflows: &[Workflow],
) -> Vec<dataflow_rs::Workflow> {
    // Index workflows by workflow_id for fast lookup
    let mut workflow_map: HashMap<String, Vec<&Workflow>> = HashMap::new();
    for workflow in workflows {
        workflow_map
            .entry(workflow.workflow_id.clone())
            .or_default()
            .push(workflow);
    }

    let mut result = Vec::new();

    for channel in channels {
        let Some(ref wf_id) = channel.workflow_id else {
            tracing::warn!(
                channel_id = %channel.channel_id,
                channel_name = %channel.name,
                "Channel has no workflow_id, skipping"
            );
            continue;
        };

        let Some(wf_versions) = workflow_map.get(wf_id) else {
            tracing::warn!(
                channel_id = %channel.channel_id,
                workflow_id = %wf_id,
                "Workflow not found for channel, skipping"
            );
            continue;
        };

        if wf_versions.len() == 1 && wf_versions[0].rollout_percentage == 100 {
            // Single version at 100% — convert normally
            match workflow_to_dataflow(wf_versions[0], &channel.name) {
                Ok(w) => result.push(w),
                Err(e) => {
                    tracing::warn!(
                        workflow_id = %wf_id,
                        channel = %channel.name,
                        error = %e,
                        "Failed to convert workflow to dataflow, skipping"
                    );
                }
            }
        } else {
            // Multiple versions or partial rollout — wrap with bucket ranges
            let mut sorted: Vec<&&Workflow> = wf_versions.iter().collect();
            sorted.sort_by(|a, b| b.version.cmp(&a.version));

            let mut bucket_offset = 0i64;
            for wf in &sorted {
                let bucket_min = bucket_offset;
                let bucket_max = bucket_offset + wf.rollout_percentage;
                match workflow_to_dataflow_with_rollout(wf, &channel.name, bucket_min, bucket_max) {
                    Ok(w) => result.push(w),
                    Err(e) => {
                        tracing::warn!(
                            workflow_id = %wf.workflow_id,
                            version = wf.version,
                            channel = %channel.name,
                            error = %e,
                            "Failed to convert workflow version to dataflow, skipping"
                        );
                    }
                }
                bucket_offset = bucket_max;
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("orders", "orders"));
        assert!(!glob_match("orders", "events"));
    }

    #[test]
    fn test_glob_match_prefix_wildcard() {
        assert!(glob_match("internal-*", "internal-debug"));
        assert!(glob_match("internal-*", "internal-"));
        assert!(!glob_match("internal-*", "external-debug"));
    }

    #[test]
    fn test_glob_match_suffix_wildcard() {
        assert!(glob_match("*-debug", "internal-debug"));
        assert!(!glob_match("*-debug", "internal-prod"));
    }

    #[test]
    fn test_glob_match_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_glob_match_middle_wildcard() {
        assert!(glob_match("pre*suf", "presuf"));
        assert!(glob_match("pre*suf", "pre-middle-suf"));
        assert!(!glob_match("pre*suf", "pre-middle"));
    }

    fn make_channel(name: &str) -> Channel {
        Channel {
            channel_id: name.to_string(),
            name: name.to_string(),
            version: 1,
            status: "active".to_string(),
            channel_type: "sync".to_string(),
            protocol: "http".to_string(),
            methods: Some("POST".to_string()),
            workflow_id: None,
            topic: None,
            consumer_group: None,
            route_pattern: None,
            description: None,
            transport_config_json: "{}".to_string(),
            config_json: "{}".to_string(),
            priority: 0,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        }
    }

    #[test]
    fn test_filter_channels_no_config() {
        let channels = vec![make_channel("orders"), make_channel("events")];
        let config = crate::config::ChannelLoadingConfig::default();
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_channels_include_only() {
        let channels = vec![
            make_channel("orders"),
            make_channel("events"),
            make_channel("internal-debug"),
        ];
        let config = crate::config::ChannelLoadingConfig {
            include: vec!["orders".to_string(), "events".to_string()],
            exclude: vec![],
        };
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|c| c.name != "internal-debug"));
    }

    #[test]
    fn test_filter_channels_exclude_only() {
        let channels = vec![
            make_channel("orders"),
            make_channel("events"),
            make_channel("internal-debug"),
        ];
        let config = crate::config::ChannelLoadingConfig {
            include: vec![],
            exclude: vec!["internal-*".to_string()],
        };
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|c| c.name != "internal-debug"));
    }

    #[test]
    fn test_filter_channels_include_and_exclude() {
        let channels = vec![
            make_channel("orders"),
            make_channel("orders-debug"),
            make_channel("events"),
        ];
        let config = crate::config::ChannelLoadingConfig {
            include: vec!["orders*".to_string()],
            exclude: vec!["*-debug".to_string()],
        };
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "orders");
    }
}
