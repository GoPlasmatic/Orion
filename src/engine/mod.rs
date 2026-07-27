pub mod functions;
pub mod profile;
pub mod utils;

use std::collections::HashMap;
use std::sync::Arc;

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
    let elapsed = start.elapsed();
    crate::metrics::record_engine_lock_wait("read", elapsed.as_secs_f64());
    profile::record_engine_lock_wait(elapsed);
    guard.clone()
}

/// Result of one engine invocation: the engine's own result plus the captured
/// per-task `ExecutionTrace` when the caller opted in (A2).
pub type EngineCallResult = (dataflow_rs::Result<()>, Option<dataflow_rs::ExecutionTrace>);

/// Run the engine for `channel` with optional timeout, optional per-task
/// trace capture, and optional profiling scope. The sync HTTP, async trace
/// queue, and Kafka ingress paths all go through here so timeout and trace
/// semantics cannot drift between them. `Err(ms)` means the call timed out
/// after `ms` milliseconds.
pub async fn run_for_channel(
    engine: &Arc<dataflow_rs::Engine>,
    channel: &str,
    message: &mut dataflow_rs::Message,
    timeout_ms: Option<u64>,
    profile: Option<&Arc<profile::ProfileCollector>>,
    capture_trace: bool,
) -> Result<EngineCallResult, u64> {
    let run = run_for_channel_inner(engine, channel, message, timeout_ms, capture_trace);
    if let Some(p) = profile {
        profile::ORION_PROFILE.scope(p.clone(), run).await
    } else {
        run.await
    }
}

async fn run_for_channel_inner(
    engine: &Arc<dataflow_rs::Engine>,
    channel: &str,
    message: &mut dataflow_rs::Message,
    timeout_ms: Option<u64>,
    capture_trace: bool,
) -> Result<EngineCallResult, u64> {
    use std::time::Duration;

    if capture_trace {
        let fut = engine.process_message_for_channel_with_trace(channel, message);
        let inner = if let Some(ms) = timeout_ms {
            match tokio::time::timeout(Duration::from_millis(ms), fut).await {
                Ok(r) => r,
                Err(_) => return Err(ms),
            }
        } else {
            fut.await
        };
        match inner {
            Ok(trace) => Ok((Ok(()), Some(trace))),
            Err(e) => Ok((Err(e), None)),
        }
    } else if let Some(ms) = timeout_ms {
        match tokio::time::timeout(
            Duration::from_millis(ms),
            engine.process_message_for_channel(channel, message),
        )
        .await
        {
            Ok(inner) => Ok((inner, None)),
            Err(_) => Err(ms),
        }
    } else {
        Ok((
            engine.process_message_for_channel(channel, message).await,
            None,
        ))
    }
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
    "data_query",
    "data_write",
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
    "data_query",
    "data_write",
    "cache_read",
    "cache_write",
    "mongo_read",
];

/// Build the custom function handlers for the dataflow-rs engine.
///
/// Registers the seven Orion-specific handlers (`http_call`, `channel_call`,
/// `db_read`, `db_write`, `cache_read`, `cache_write`, `mongo_read`) plus a
/// stub `publish_kafka`. Call [`register_kafka_publisher`] afterwards to swap
/// the stub for the real Kafka-backed handler once the producer is initialised.
#[allow(clippy::too_many_arguments)]
pub fn build_custom_functions(
    registry: Arc<ConnectorRegistry>,
    client: reqwest::Client,
    engine: Arc<tokio::sync::RwLock<Arc<dataflow_rs::Engine>>>,
    channel_registry: Arc<crate::channel::ChannelRegistry>,
    engine_config: &crate::config::EngineConfig,
    query_config: &crate::config::QueryConfig,
    write_config: &crate::config::WriteConfig,
    cache_pool: Arc<crate::connector::cache_backend::CachePool>,
    sql_pool_cache: Arc<crate::connector::pool_cache::SqlPoolCache>,
    mongo_pool_cache: Arc<crate::connector::mongo_pool::MongoPoolCache>,
) -> HashMap<String, dataflow_rs::BoxedFunctionHandler> {
    let mut fns: HashMap<String, dataflow_rs::BoxedFunctionHandler> = HashMap::new();

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
            channel_registry,
            max_call_depth: engine_config.max_channel_call_depth,
            default_timeout_ms: engine_config.default_channel_call_timeout_ms,
        }),
    );

    // Register stub publish_kafka (will be replaced by register_kafka_publisher when Kafka is configured)
    fns.insert(
        "publish_kafka".to_string(),
        Box::new(functions::publish_kafka::PublishKafkaHandler {
            registry: registry.clone(),
            producers: None,
        }),
    );

    // Register SQL database handlers (db_read, db_write)
    fns.insert(
        "db_read".to_string(),
        Box::new(functions::db_read::DbReadHandler {
            pool_cache: sql_pool_cache.clone(),
            registry: registry.clone(),
            max_rows: query_config.max_limit as usize,
        }),
    );
    fns.insert(
        "db_write".to_string(),
        Box::new(functions::db_write::DbWriteHandler {
            pool_cache: sql_pool_cache.clone(),
            registry: registry.clone(),
        }),
    );

    // Register the portable query handler (data_query). It renders a
    // backend-neutral filter + envelope to native SQL or a MongoDB find
    // (§ src/query/).
    fns.insert(
        "data_query".to_string(),
        Box::new(functions::data_query::DataQueryHandler {
            pool_cache: sql_pool_cache.clone(),
            mongo_pool_cache: mongo_pool_cache.clone(),
            http_client: client.clone(),
            registry: registry.clone(),
            default_limit: query_config.default_limit,
            max_limit: query_config.max_limit,
        }),
    );

    // Register the portable write handler (data_write). It renders a
    // backend-neutral mutation envelope to a native SQL INSERT/UPDATE/DELETE/upsert,
    // a MongoDB write, or an Elasticsearch write (§ src/query/write.rs).
    fns.insert(
        "data_write".to_string(),
        Box::new(functions::data_write::DataWriteHandler {
            pool_cache: sql_pool_cache,
            mongo_pool_cache: mongo_pool_cache.clone(),
            http_client: client.clone(),
            registry: registry.clone(),
            max_rows: write_config.max_rows,
            allow_unfiltered: write_config.allow_unfiltered,
        }),
    );

    // Register cache handlers (cache_read, cache_write).
    // CachePool routes to the in-memory or Redis backend per connector config.
    fns.insert(
        "cache_read".to_string(),
        Box::new(functions::cache_read::CacheReadHandler {
            cache_pool: cache_pool.clone(),
            registry: registry.clone(),
        }),
    );
    fns.insert(
        "cache_write".to_string(),
        Box::new(functions::cache_write::CacheWriteHandler {
            cache_pool,
            registry: registry.clone(),
        }),
    );

    // Register MongoDB handler (mongo_read)
    fns.insert(
        "mongo_read".to_string(),
        Box::new(functions::mongo_read::MongoReadHandler {
            pool_cache: mongo_pool_cache,
            registry: registry.clone(),
            max_rows: query_config.max_limit as usize,
        }),
    );

    fns
}

/// Register the real Kafka-backed publish_kafka handler.
///
/// Replaces the stub handler (or adds the handler if not yet registered).
pub fn register_kafka_publisher(
    fns: &mut HashMap<String, dataflow_rs::BoxedFunctionHandler>,
    registry: Arc<ConnectorRegistry>,
    producers: Arc<crate::kafka::producer::KafkaProducerCache>,
) {
    fns.insert(
        "publish_kafka".to_string(),
        Box::new(functions::publish_kafka::PublishKafkaHandler {
            registry,
            producers: Some(producers),
        }),
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

/// Glob matching supporting `*` wildcards, with real backtracking.
///
/// F32: the previous successive-`find` implementation diverged from glob
/// semantics on patterns needing backtracking (`a*bc` vs `abxbc` matched the
/// first `b` and failed) — and a wrong `channels.include`/`exclude` pattern
/// silently drops a channel. This is the standard two-pointer matcher:
/// linear scan with a single saved star position to retry from.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            // Backtrack: let the last `*` swallow one more character.
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Convert active channels and their workflows to dataflow-rs workflows for the engine.
///
/// For each active channel, finds the associated workflow(s) and builds
/// dataflow-rs Workflow objects with the channel name injected as the channel field.
///
/// F33: a channel whose workflows cannot be built — missing `workflow_id`,
/// workflow not found among the active set, or a version that fails
/// conversion — is reported as a [`ChannelLoadIssue`] instead of being
/// silently skipped. Callers feed these into `ChannelRegistry::reload`, which
/// quarantines the channel: previously it stayed registered in the route
/// table with no workflow behind it, so requests got an opaque engine error.
pub fn build_engine_workflows(
    channels: &[Channel],
    workflows: &[Workflow],
) -> (
    Vec<dataflow_rs::Workflow>,
    Vec<crate::channel::ChannelLoadIssue>,
) {
    // Index workflows by workflow_id for fast lookup
    let mut workflow_map: HashMap<String, Vec<&Workflow>> = HashMap::new();
    for workflow in workflows {
        workflow_map
            .entry(workflow.workflow_id.clone())
            .or_default()
            .push(workflow);
    }

    let mut result = Vec::new();
    let mut issues: Vec<crate::channel::ChannelLoadIssue> = Vec::new();

    for channel in channels {
        let Some(ref wf_id) = channel.workflow_id else {
            issues.push(crate::channel::ChannelLoadIssue {
                channel: channel.name.clone(),
                reason: "channel has no workflow_id".to_string(),
            });
            continue;
        };

        let Some(wf_versions) = workflow_map.get(wf_id) else {
            issues.push(crate::channel::ChannelLoadIssue {
                channel: channel.name.clone(),
                reason: format!("workflow '{wf_id}' not found among active workflows"),
            });
            continue;
        };

        if wf_versions.len() == 1 && wf_versions[0].rollout_percentage == 100 {
            // Single version at 100% — convert normally
            match workflow_to_dataflow(wf_versions[0], &channel.name) {
                Ok(w) => result.push(w),
                Err(e) => {
                    issues.push(crate::channel::ChannelLoadIssue {
                        channel: channel.name.clone(),
                        reason: format!("workflow '{wf_id}' failed to convert: {e}"),
                    });
                }
            }
        } else {
            // Multiple versions or partial rollout — wrap with bucket ranges
            let mut sorted: Vec<&&Workflow> = wf_versions.iter().collect();
            sorted.sort_by_key(|b| std::cmp::Reverse(b.version));

            let mut bucket_offset = 0i64;
            let mut converted = Vec::new();
            let mut failed = false;
            for wf in &sorted {
                let bucket_min = bucket_offset;
                let bucket_max = bucket_offset + wf.rollout_percentage;
                match workflow_to_dataflow_with_rollout(wf, &channel.name, bucket_min, bucket_max) {
                    Ok(w) => converted.push(w),
                    Err(e) => {
                        issues.push(crate::channel::ChannelLoadIssue {
                            channel: channel.name.clone(),
                            reason: format!(
                                "workflow '{}' v{} failed to convert: {e}",
                                wf.workflow_id, wf.version
                            ),
                        });
                        failed = true;
                        break;
                    }
                }
                bucket_offset = bucket_max;
            }
            // F30: buckets are 0–99; percentages that don't sum to 100
            // silently misroute — under 100, the remainder of the traffic
            // matches no workflow version at all; over 100, later versions'
            // ranges start past bucket 99 and are unreachable.
            if !failed && bucket_offset != 100 {
                issues.push(crate::channel::ChannelLoadIssue {
                    channel: channel.name.clone(),
                    reason: format!(
                        "rollout percentages for workflow '{wf_id}' sum to {bucket_offset}, \
                         not 100 — {}",
                        if bucket_offset < 100 {
                            format!(
                                "{}% of traffic would match no workflow version",
                                100 - bucket_offset
                            )
                        } else {
                            "later versions would be unreachable".to_string()
                        }
                    ),
                });
                failed = true;
            }
            // All-or-nothing per channel: a partially-converted rollout would
            // silently blackhole the failed version's bucket range.
            if !failed {
                result.append(&mut converted);
            }
        }
    }

    (result, issues)
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

    /// F32: cases the old successive-`find` matcher got wrong.
    #[test]
    fn test_glob_match_backtracking() {
        // The old matcher bound `b` to the first occurrence and failed.
        assert!(glob_match("a*bc", "abxbc"));
        assert!(glob_match("a*bc", "abcbc"));
        assert!(!glob_match("a*bc", "abxbd"));
    }

    #[test]
    fn test_glob_match_multi_star() {
        assert!(glob_match("a*b*c", "a-x-b-y-c"));
        assert!(glob_match("*orders*", "internal-orders-debug"));
        assert!(!glob_match("a*b*c", "a-x-c-y-b"));
        assert!(glob_match("**", "anything"));
    }

    fn make_channel(name: &str) -> Channel {
        Channel {
            channel_id: name.to_string(),
            name: name.to_string(),
            version: 1,
            status: crate::storage::models::EntityStatus::Active
                .as_str()
                .to_string(),
            channel_type: "sync".to_string(),
            protocol: crate::storage::models::ChannelProtocol::Http
                .as_str()
                .to_string(),
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

    fn make_workflow(wf_id: &str, version: i64, rollout: i64) -> Workflow {
        Workflow {
            workflow_id: wf_id.to_string(),
            version,
            name: format!("{wf_id}-v{version}"),
            description: None,
            priority: 0,
            status: "active".to_string(),
            rollout_percentage: rollout,
            condition_json: "true".to_string(),
            tasks_json:
                r#"[{"id":"t1","name":"log","function":{"name":"log","input":{"message":"x"}}}]"#
                    .to_string(),
            tags: "[]".to_string(),
            continue_on_error: false,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        }
    }

    /// F30: rollout percentages that don't sum to 100 quarantine the channel
    /// instead of silently blackholing (or shadowing) part of the traffic.
    #[test]
    fn test_rollout_sum_must_be_100() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());

        // 50 + 30 = 80 — buckets 80–99 would match no version.
        let wfs = vec![make_workflow("wf", 1, 30), make_workflow("wf", 2, 50)];
        let (converted, issues) = build_engine_workflows(&[channel.clone()], &wfs);
        assert!(converted.is_empty(), "under-100 rollout must not half-load");
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("sum to 80"),
            "{}",
            issues[0].reason
        );

        // 60 + 60 = 120 — later versions unreachable.
        let wfs = vec![make_workflow("wf", 1, 60), make_workflow("wf", 2, 60)];
        let (converted, issues) = build_engine_workflows(&[channel.clone()], &wfs);
        assert!(converted.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("sum to 120"),
            "{}",
            issues[0].reason
        );

        // 50 + 50 = 100 — loads both versions cleanly.
        let wfs = vec![make_workflow("wf", 1, 50), make_workflow("wf", 2, 50)];
        let (converted, issues) = build_engine_workflows(&[channel], &wfs);
        assert_eq!(converted.len(), 2);
        assert!(issues.is_empty(), "{issues:?}");
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
