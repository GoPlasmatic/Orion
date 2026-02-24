pub mod functions;
pub mod utils;

use std::collections::HashMap;
use std::sync::Arc;

use dataflow_rs::engine::functions::AsyncFunctionHandler;

use crate::connector::ConnectorRegistry;

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
    "enrich",
    "publish_kafka",
];

/// Function names that require a connector reference.
pub const CONNECTOR_FUNCTIONS: &[&str] = &["http_call", "enrich", "publish_kafka"];

/// Build the custom function handlers for the dataflow-rs engine.
///
/// Registers http_call, enrich, and (when kafka feature is disabled) a stub
/// publish_kafka handler. Use [`upgrade_publish_kafka`] to register the real
/// Kafka-backed handler when the feature is enabled.
pub fn build_custom_functions(
    registry: Arc<ConnectorRegistry>,
    client: reqwest::Client,
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
        "enrich".to_string(),
        Box::new(functions::enrich::EnrichHandler {
            registry: registry.clone(),
            client,
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

/// Convert a list of rules to workflows, logging warnings for failures.
pub fn build_engine_workflows(
    rules: &[crate::storage::models::Rule],
) -> Vec<dataflow_rs::Workflow> {
    let mut workflows = Vec::new();
    for rule in rules {
        match crate::storage::repositories::rules::rule_to_workflow(rule) {
            Ok(w) => workflows.push(w),
            Err(e) => {
                tracing::warn!(rule_id = %rule.id, error = %e, "Failed to convert rule to workflow, skipping");
            }
        }
    }
    workflows
}
