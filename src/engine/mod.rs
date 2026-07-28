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
/// Registers the nine Orion-specific handlers (`http_call`, `channel_call`,
/// `db_read`, `db_write`, `data_query`, `data_write`, `cache_read`,
/// `cache_write`, `mongo_read`) plus a stub `publish_kafka`. Call
/// [`register_kafka_publisher`] afterwards to swap the stub for the real
/// Kafka-backed handler once the producer is initialised.
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

/// Filter channels based on include/exclude glob patterns from [`ChannelFilterConfig`].
///
/// - If `include` is non-empty, only channels matching at least one include pattern are kept.
/// - Channels matching any `exclude` pattern are removed (applied after include).
/// - Supports simple `*` wildcards (e.g., `internal-*`, `*-debug`).
pub fn filter_channels(
    channels: Vec<Channel>,
    config: &crate::config::ChannelFilterConfig,
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
/// Every function name registered by [`build_custom_functions`].
///
/// A task naming anything else lands in `FunctionConfig::Custom` and makes
/// `precompile_custom_inputs` fail with `FunctionNotFound` — which aborts the
/// *whole* engine build. Checking membership here downgrades that to a
/// per-channel quarantine (proposal F41). Kept next to `build_custom_functions`;
/// `registered_handler_names_match_the_constant` pins the two together.
pub const CUSTOM_HANDLER_FUNCTIONS: &[&str] = &[
    "cache_read",
    "cache_write",
    "channel_call",
    "data_query",
    "data_write",
    "db_read",
    "db_write",
    "http_call",
    "mongo_read",
    "publish_kafka",
];

/// Run the same `serde` deserialization that `dataflow_rs`'s
/// `precompile_custom_inputs` will run at engine construction.
///
/// `AsyncFunctionHandler::parse_input_box` delegates to
/// `serde_json::from_value::<Self::Input>`, which is purely type-driven — no
/// handler state is consulted — so checking the type here is equivalent to
/// checking it there. The engine's own handler map is not reachable once the
/// engine exists (`Engine::new` consumes it and the field is private), which is
/// why this is a table rather than a lookup.
///
/// Only `channel_call` has a typed `Input`; every other Orion handler takes
/// `serde_json::Value` and accepts any JSON. (`http_call` and `publish_kafka`
/// are dataflow-rs *builtins* — their typed configs are already parsed during
/// `workflow_to_dataflow`, so they never reach `Custom`.)
fn custom_input_parse_check(name: &str, input: &serde_json::Value) -> Result<(), String> {
    fn check<T: serde::de::DeserializeOwned>(v: &serde_json::Value) -> Result<(), String> {
        serde_json::from_value::<T>(v.clone())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    match name {
        "channel_call" => check::<functions::channel_call::ChannelCallInput>(input),
        _ => Ok(()),
    }
}

/// Validate every custom task before the engine is built.
///
/// Without this, an unregistered function name or a typed-input mismatch is
/// invisible until engine construction — which is a whole-instance failure, not
/// a per-channel one: at boot the process aborts, and on reload every channel on
/// every node goes down because one stored row is unusable. That defeats the
/// F33/F35 quarantine, whose premise is that one broken row must never stop the
/// instance. Checking here turns it back into a `ChannelLoadIssue` (F41).
fn check_custom_inputs(wf: &dataflow_rs::Workflow) -> Result<(), String> {
    use dataflow_rs::engine::functions::config::FunctionConfig;
    for task in &wf.tasks {
        let FunctionConfig::Custom { name, input, .. } = &task.function else {
            continue;
        };
        if !CUSTOM_HANDLER_FUNCTIONS.contains(&name.as_str()) {
            return Err(format!(
                "task '{}' calls unregistered function '{name}'",
                task.id
            ));
        }
        custom_input_parse_check(name, input)
            .map_err(|e| format!("task '{}' has invalid input for '{name}': {e}", task.id))?;
    }
    Ok(())
}

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
                Ok(w) => match check_custom_inputs(&w) {
                    Ok(()) => result.push(w),
                    Err(e) => {
                        issues.push(crate::channel::ChannelLoadIssue {
                            channel: channel.name.clone(),
                            reason: format!("workflow '{wf_id}' has an unusable task: {e}"),
                        });
                    }
                },
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
                match workflow_to_dataflow_with_rollout(wf, &channel.name, bucket_min, bucket_max)
                    .map_err(|e| e.to_string())
                    .and_then(|w| check_custom_inputs(&w).map(|()| w))
                {
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

    /// Extract the `[min, max)` rollout bucket range from a converted
    /// workflow's wrapped condition (`and[1]` is `>= min`, `and[2]` is `< max`).
    fn bucket_range(wf: &dataflow_rs::Workflow) -> (i64, i64) {
        let and = wf.condition["and"].as_array().expect("wrapped condition");
        (
            and[1][">="][1].as_i64().expect("bucket_min"),
            and[2]["<"][1].as_i64().expect("bucket_max"),
        )
    }

    /// The bucket ranges must partition 0–99 contiguously, newest version
    /// first: 20/50/30 across v3/v2/v1 → v3 [0,20), v2 [20,70), v1 [70,100).
    /// A gap or overlap in the offsets silently misroutes traffic, which is
    /// exactly what this accumulation arithmetic exists to prevent.
    #[test]
    fn test_rollout_bucket_offsets_partition_newest_first() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());
        let wfs = vec![
            make_workflow("wf", 1, 30),
            make_workflow("wf", 2, 50),
            make_workflow("wf", 3, 20),
        ];
        let (converted, issues) = build_engine_workflows(&[channel], &wfs);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(converted.len(), 3);

        let by_id: std::collections::HashMap<String, (i64, i64)> = converted
            .iter()
            .map(|w| (w.id.clone(), bucket_range(w)))
            .collect();
        assert_eq!(
            by_id["wf:v3"],
            (0, 20),
            "newest version gets the first bucket"
        );
        assert_eq!(by_id["wf:v2"], (20, 70));
        assert_eq!(by_id["wf:v1"], (70, 100));
    }

    /// F33: a channel with no workflow_id, and one pointing at a workflow
    /// that is not in the active set, must each surface a load issue naming
    /// the problem rather than being silently skipped.
    #[test]
    fn test_missing_and_unknown_workflow_are_reported_as_issues() {
        let no_wf = make_channel("no-wf");
        let mut unknown = make_channel("unknown-wf");
        unknown.workflow_id = Some("ghost".to_string());

        let (converted, issues) = build_engine_workflows(&[no_wf, unknown], &[]);
        assert!(converted.is_empty());
        assert_eq!(issues.len(), 2);
        assert!(
            issues[0].reason.contains("no workflow_id"),
            "{}",
            issues[0].reason
        );
        assert!(
            issues[1].reason.contains("'ghost' not found"),
            "{}",
            issues[1].reason
        );
    }

    /// All-or-nothing per channel: when one version of a rollout fails to
    /// convert, the versions that DID convert must not load — a partial
    /// rollout would silently blackhole the failed version's bucket range.
    #[test]
    fn test_partial_rollout_conversion_failure_loads_nothing() {
        let mut channel = make_channel("rollout-ch");
        channel.workflow_id = Some("wf".to_string());

        // v2 (processed first, newest) converts fine; v1 has broken tasks.
        let mut bad_v1 = make_workflow("wf", 1, 50);
        bad_v1.tasks_json = "not json".to_string();
        let wfs = vec![bad_v1, make_workflow("wf", 2, 50)];

        let (converted, issues) = build_engine_workflows(&[channel], &wfs);
        assert!(
            converted.is_empty(),
            "the successfully-converted v2 must be discarded with v1"
        );
        assert_eq!(issues.len(), 1);
        assert!(
            issues[0].reason.contains("v1 failed to convert"),
            "{}",
            issues[0].reason
        );
    }

    #[test]
    fn test_filter_channels_no_config() {
        let channels = vec![make_channel("orders"), make_channel("events")];
        let config = crate::config::ChannelFilterConfig::default();
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
        let config = crate::config::ChannelFilterConfig {
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
        let config = crate::config::ChannelFilterConfig {
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
        let config = crate::config::ChannelFilterConfig {
            include: vec!["orders*".to_string()],
            exclude: vec!["*-debug".to_string()],
        };
        let filtered = filter_channels(channels, &config);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "orders");
    }

    // -- F41: a malformed task input quarantines its channel, not the engine --

    fn workflow_with_raw_tasks(wf_id: &str, tasks_json: &str) -> Workflow {
        let mut wf = make_workflow(wf_id, 1, 100);
        wf.tasks_json = tasks_json.to_string();
        wf
    }

    #[test]
    fn unregistered_custom_function_quarantines_only_its_channel() {
        // A task naming a function that is neither a dataflow-rs builtin nor a
        // registered Orion handler lands in FunctionConfig::Custom, where
        // precompile_custom_inputs fails with FunctionNotFound — aborting the
        // *entire* engine build. Before F41 that meant boot aborted, or a reload
        // took down every channel on every node, because of one stored row.
        let mut good = make_channel("good");
        good.workflow_id = Some("wf-good".to_string());
        let mut bad = make_channel("bad");
        bad.workflow_id = Some("wf-bad".to_string());

        let wfs = vec![
            make_workflow("wf-good", 1, 100),
            workflow_with_raw_tasks(
                "wf-bad",
                r#"[{"id":"t1","name":"oops","function":{"name":"totally_not_a_function",
                   "input":{}}}]"#,
            ),
        ];

        let (converted, issues) = build_engine_workflows(&[good, bad], &wfs);

        assert_eq!(
            converted.len(),
            1,
            "the healthy channel must still be built"
        );
        assert_eq!(converted[0].channel, "good");
        assert_eq!(issues.len(), 1, "issues = {issues:?}");
        assert_eq!(issues[0].channel, "bad");
        assert!(
            issues[0]
                .reason
                .contains("unregistered function 'totally_not_a_function'"),
            "reason should name the offending function: {}",
            issues[0].reason
        );
    }

    #[test]
    fn channel_call_with_only_channel_logic_builds() {
        // F23: the schema (and the docs) declare `channel` optional when
        // `channel_logic` is given, but the struct required it — so this exact
        // workflow passed admin validation and then failed the engine build.
        let mut channel = make_channel("dyn");
        channel.workflow_id = Some("wf-dyn".to_string());
        let wfs = vec![workflow_with_raw_tasks(
            "wf-dyn",
            r#"[{"id":"t1","name":"fan","function":{"name":"channel_call",
               "input":{"channel_logic":{"var":"target"}}}}]"#,
        )];

        let (converted, issues) = build_engine_workflows(&[channel], &wfs);
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
        assert_eq!(converted.len(), 1);
    }

    #[test]
    fn channel_call_input_rejects_a_wrongly_typed_field() {
        // `channel` is defaulted (F23) but still typed: a non-string must not
        // slip through to the engine build.
        assert!(
            custom_input_parse_check("channel_call", &serde_json::json!({ "channel": 7 })).is_err()
        );
        assert!(
            custom_input_parse_check("channel_call", &serde_json::json!({ "channel": "a" }))
                .is_ok()
        );
        // A `Value`-input handler accepts anything.
        assert!(custom_input_parse_check("db_read", &serde_json::json!({ "x": 1 })).is_ok());
    }

    #[test]
    fn known_functions_covers_every_registered_custom_handler() {
        // A name registered as a handler but missing from KNOWN_FUNCTIONS is
        // rejected by admin validation even though it would work; the reverse
        // (in KNOWN_FUNCTIONS, unregistered, not a builtin) reaches the engine
        // build and kills it. Both directions must hold.
        for name in CUSTOM_HANDLER_FUNCTIONS {
            assert!(
                KNOWN_FUNCTIONS.contains(name),
                "handler '{name}' is registered but absent from KNOWN_FUNCTIONS, \
                 so workflows using it are rejected at activation"
            );
        }
    }
}
